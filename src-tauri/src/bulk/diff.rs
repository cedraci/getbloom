//! Diffing a parsed sheet against the registry.
//!
//! This file is deliberately pure: no database, no filesystem, no spreadsheet
//! crate. Every interesting decision in the bulk import -- what counts as an
//! edit, when a missing row is a removal, which rows are invalid -- is decided
//! here and therefore testable in milliseconds without Postgres or Excel.

use crate::bulk::sheet::SheetData;
use crate::resolution::normalize::{build_security, detect_id_kind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// An instrument as the book currently holds it, flattened to names so the
/// differ never has to resolve an id to a class or a view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DbInstrument {
    pub instrument_id: i64,
    pub label: String,
    pub class: String,
    pub identifier: String,
    pub yellow_key: String,
    pub active: bool,
    pub security: String,
    pub views: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetRef {
    pub id: i64,
    pub label: String,
    pub security: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddRow {
    pub row_number: u32,
    pub label: String,
    pub class: String,
    pub identifier: String,
    pub yellow_key: String,
    pub active: bool,
    /// Resolved here so the plan is legible in a preview; the apply step
    /// re-resolves for real through `book::add`, which is the only place an
    /// instrument_id is actually minted.
    pub security: String,
    pub views: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditRow {
    pub id: i64,
    pub row_number: u32,
    pub label: String,
    pub class: String,
    pub identifier: String,
    pub yellow_key: String,
    pub security: String,
    /// Column names that differ from the database, for display.
    pub changed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MembershipChange {
    pub id: i64,
    pub label: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvalidRow {
    pub row_number: u32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportPlan {
    pub file_hash: String,
    pub has_id_column: bool,
    pub adds: Vec<AddRow>,
    pub edits: Vec<EditRow>,
    pub retires: Vec<AssetRef>,
    pub reactivations: Vec<AssetRef>,
    pub membership_changes: Vec<MembershipChange>,
    pub removals: Vec<AssetRef>,
    pub invalid_rows: Vec<InvalidRow>,
    pub active_asset_count: i64,
    pub requires_typed_confirmation: bool,
}

impl ImportPlan {
    pub fn is_empty(&self) -> bool {
        self.adds.is_empty()
            && self.edits.is_empty()
            && self.retires.is_empty()
            && self.reactivations.is_empty()
            && self.membership_changes.is_empty()
            && self.removals.is_empty()
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

pub fn diff(
    sheet: &SheetData,
    db: &[DbInstrument],
    known_classes: &[String],
    known_views: &[String],
    file_hash: &str,
) -> ImportPlan {
    let mut plan = ImportPlan {
        file_hash: file_hash.to_string(),
        has_id_column: sheet.has_id_column,
        adds: vec![],
        edits: vec![],
        retires: vec![],
        reactivations: vec![],
        membership_changes: vec![],
        removals: vec![],
        invalid_rows: vec![],
        active_asset_count: db.iter().filter(|a| a.active).count() as i64,
        requires_typed_confirmation: false,
    };

    let classes: HashSet<&str> = known_classes.iter().map(String::as_str).collect();
    let views: HashSet<&str> = known_views.iter().map(String::as_str).collect();
    let by_id: HashMap<i64, &DbInstrument> = db.iter().map(|a| (a.instrument_id, a)).collect();

    // A view with no column in this sheet is a view the sheet does not speak
    // for. Its memberships must be left alone, symmetric with guardrail 1 (a
    // sheet with no id column cannot speak for rows it never lists). Without
    // this, a view created after the file was exported -- absent from every
    // column, therefore absent from every row -- would read as "removed from"
    // for every asset that happens to belong to it.
    let sheet_views: HashSet<&str> = sheet.view_columns.iter().map(String::as_str).collect();

    // Header problems are attributed to row 1, which is where Excel shows them.
    for v in &sheet.view_columns {
        if !views.contains(v.as_str()) {
            plan.invalid_rows.push(InvalidRow {
                row_number: 1,
                reason: format!("column '{v}' names a view that does not exist"),
            });
        }
    }

    // Securities claimed by rows in this sheet, used to catch a rename that
    // would collide with another instrument's identity.
    let mut claimed: HashMap<String, u32> = HashMap::new();
    let mut seen_ids: HashSet<i64> = HashSet::new();
    let mut present_ids: HashSet<i64> = HashSet::new();

    for r in &sheet.rows {
        // Identity comes before every other check, including label and class.
        // A row carrying a real instrument_id must register as "still
        // present" no matter what else is wrong with it, or a validation
        // failure elsewhere (a class typo) gets misread by the removal pass
        // below as "the user deleted this instrument" -- exactly what
        // guardrail 1 exists to prevent.
        let existing = match r.instrument_id {
            None => None,
            Some(id) => {
                if !seen_ids.insert(id) {
                    plan.invalid_rows.push(InvalidRow {
                        row_number: r.row_number,
                        reason: format!("instrument_id {id} appears twice in the sheet"),
                    });
                    continue;
                }
                let Some(cur) = by_id.get(&id) else {
                    plan.invalid_rows.push(InvalidRow {
                        row_number: r.row_number,
                        reason: format!("instrument_id {id} is not in the database"),
                    });
                    continue;
                };
                present_ids.insert(id);
                Some(*cur)
            }
        };

        if r.label.is_empty() {
            plan.invalid_rows.push(InvalidRow {
                row_number: r.row_number,
                reason: "label is empty".into(),
            });
            continue;
        }
        if !classes.contains(r.class.as_str()) {
            plan.invalid_rows.push(InvalidRow {
                row_number: r.row_number,
                reason: format!("class '{}' does not exist", r.class),
            });
            continue;
        }

        let security = match build_security(detect_id_kind(&r.identifier), &r.identifier, &r.yellow_key) {
            Ok(s) => s,
            Err(e) => {
                plan.invalid_rows.push(InvalidRow {
                    row_number: r.row_number,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        if let Some(first) = claimed.get(&security) {
            plan.invalid_rows.push(InvalidRow {
                row_number: r.row_number,
                reason: format!("security '{security}' is already claimed by row {first}"),
            });
            continue;
        }

        // A security that belongs to a DIFFERENT instrument means this row is
        // trying to rebind an existing instrument's identity onto one it does
        // not own. The same instrument keeping its own security is fine.
        if let Some(owner) = db.iter().find(|a| a.security == security) {
            if Some(owner.instrument_id) != r.instrument_id {
                plan.invalid_rows.push(InvalidRow {
                    row_number: r.row_number,
                    reason: format!(
                        "security '{security}' already belongs to instrument #{} '{}'; \
                         if you just imported this sheet, export it again to pick up the new ids",
                        owner.instrument_id, owner.label),
                });
                continue;
            }
        }

        // Only claim the security once the row has fully passed every check --
        // a rejected row's claim must not survive to wrongly indict a later,
        // valid row that happens to resolve to the same security.
        claimed.insert(security.clone(), r.row_number);

        let views_now = sorted(r.views.clone());

        match existing {
            None => plan.adds.push(AddRow {
                row_number: r.row_number,
                label: r.label.clone(),
                class: r.class.clone(),
                identifier: r.identifier.clone(),
                yellow_key: r.yellow_key.clone(),
                active: r.active,
                security,
                views: views_now,
            }),
            Some(cur) => {
                // Changing an existing row's identifier OR its yellow key
                // would rebind one instrument_id to a different security,
                // which destroys the link between the history already stored
                // and the instrument it belongs to. Compared on the resolved
                // security rather than the identifier alone, so a yellow-key
                // change (also an identity change: "AAPL US Equity" and
                // "AAPL US Corp" are different instruments) is caught the same
                // way. Removing the row and adding the new identifier is the
                // honest way to express that -- and the only one available:
                // book_entry stores no identity fields for an UPDATE to touch,
                // identity lives in instrument_alias, which is append-only.
                if !cur.security.eq_ignore_ascii_case(&security) {
                    plan.invalid_rows.push(InvalidRow {
                        row_number: r.row_number,
                        reason: format!(
                            "identifier cannot be changed in place (was {}, sheet says {}); \
                             remove this row and add the new identifier instead",
                            cur.identifier, r.identifier),
                    });
                    continue;
                }

                // Only `label` and `class` are book_entry's own columns and
                // therefore the only fields an edit can actually touch --
                // identifier and yellow_key together are the security, and
                // the check above has already required it to be unchanged by
                // the time this line runs.
                let id = cur.instrument_id;
                let mut changed = Vec::new();
                if r.label != cur.label { changed.push("label".to_string()); }
                if r.class != cur.class { changed.push("class".to_string()); }
                if !changed.is_empty() {
                    plan.edits.push(EditRow {
                        id,
                        row_number: r.row_number,
                        label: r.label.clone(),
                        class: r.class.clone(),
                        identifier: r.identifier.clone(),
                        yellow_key: r.yellow_key.clone(),
                        security: security.clone(),
                        changed,
                    });
                }

                // `active` is its own category, never an edit: the two ways of
                // stopping collection should read as one thing in the diff.
                let aref = AssetRef { id, label: cur.label.clone(), security };
                if r.active != cur.active {
                    if r.active { plan.reactivations.push(aref); }
                    else { plan.retires.push(aref); }
                }

                // Only views this sheet has a column for are in play. A view
                // absent from `sheet.view_columns` is not addressed by this
                // sheet, so the instrument's current membership in it is
                // dropped from `before` too and therefore never proposed as a
                // removal (or, symmetrically, an addition it could never have
                // shown).
                let before = sorted(cur.views.iter()
                    .filter(|v| sheet_views.contains(v.as_str()))
                    .cloned().collect());
                if views_now != before {
                    plan.membership_changes.push(MembershipChange {
                        id,
                        label: cur.label.clone(),
                        added: views_now.iter().filter(|v| !before.contains(v))
                            .cloned().collect(),
                        removed: before.iter().filter(|v| !views_now.contains(v))
                            .cloned().collect(),
                    });
                }
            }
        }
    }

    // Guardrail 1: only a file that came from Export -- one carrying ids -- can
    // say that a missing row means "remove this".
    if sheet.has_id_column {
        for a in db {
            if !present_ids.contains(&a.instrument_id) {
                plan.removals.push(AssetRef {
                    id: a.instrument_id,
                    label: a.label.clone(),
                    security: a.security.clone(),
                });
            }
        }
    }

    // Guardrail 2: a removal set larger than half the active book is more
    // likely a truncated paste than an intention.
    plan.requires_typed_confirmation =
        plan.active_asset_count > 0
            && (plan.removals.len() as i64) * 2 > plan.active_asset_count;

    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bulk::sheet::{SheetData, SheetRow};

    fn classes() -> Vec<String> { vec!["Equity".into(), "Corp".into()] }
    fn views() -> Vec<String> { vec!["Daily".into(), "Weekly".into()] }

    fn db_apple() -> DbInstrument {
        DbInstrument {
            instrument_id: 1, label: "Apple".into(), class: "Equity".into(),
            identifier: "AAPL US".into(), yellow_key: "Equity".into(),
            active: true, security: "AAPL US Equity".into(), views: vec!["Daily".into()],
        }
    }

    fn row_from(a: &DbInstrument) -> SheetRow {
        SheetRow {
            row_number: 2, instrument_id: Some(a.instrument_id), label: a.label.clone(),
            class: a.class.clone(), identifier: a.identifier.clone(),
            yellow_key: a.yellow_key.clone(), active: a.active, views: a.views.clone(),
        }
    }

    fn sheet(rows: Vec<SheetRow>, has_id: bool) -> SheetData {
        SheetData { has_id_column: has_id, view_columns: views(), rows }
    }

    #[test]
    fn an_unchanged_export_produces_an_empty_plan() {
        let db = vec![db_apple()];
        let s = sheet(vec![row_from(&db[0])], true);
        let p = diff(&s, &db, &classes(), &views(), "hash");
        assert!(p.adds.is_empty() && p.edits.is_empty() && p.removals.is_empty()
                && p.retires.is_empty() && p.reactivations.is_empty()
                && p.membership_changes.is_empty() && p.invalid_rows.is_empty(),
                "round trip must be a no-op, got {p:?}");
    }

    #[test]
    fn a_blank_id_is_an_add_with_a_resolved_security() {
        let db = vec![db_apple()];
        let mut new_row = row_from(&db[0]);
        new_row.instrument_id = None;
        new_row.row_number = 3;
        new_row.label = "Microsoft".into();
        new_row.identifier = "MSFT US".into();
        new_row.views = vec!["Weekly".into()];
        let p = diff(&sheet(vec![row_from(&db[0]), new_row], true), &db,
                     &classes(), &views(), "hash");
        assert_eq!(p.adds.len(), 1);
        assert_eq!(p.adds[0].label, "Microsoft");
        assert_eq!(p.adds[0].security, "MSFT US Equity");
        assert_eq!(p.adds[0].views, vec!["Weekly".to_string()]);
        assert!(p.removals.is_empty());
    }

    #[test]
    fn a_new_row_is_an_addition_that_will_need_resolving() {
        let sheet = SheetData { has_id_column: true, view_columns: vec![],
            rows: vec![SheetRow { row_number: 2, instrument_id: None,
                label: "Tesla".into(), class: "Equity".into(),
                identifier: "TSLA US".into(), yellow_key: "Equity".into(),
                active: true, views: vec![] }] };
        let plan = diff(&sheet, &[], &["Equity".into()], &[], "hash");
        assert_eq!(plan.adds.len(), 1);
        assert_eq!(plan.adds[0].identifier, "TSLA US");
    }

    /// The identifier is not editable in place: changing it means a different
    /// instrument, which is an add plus a removal, not an edit. Silently
    /// rebinding an existing instrument_id to a new security is exactly the
    /// history-destroying edit this phase exists to prevent.
    #[test]
    fn changing_the_identifier_of_an_existing_row_is_rejected() {
        let db = vec![DbInstrument { instrument_id: 7, label: "Apple".into(),
            class: "Equity".into(), identifier: "AAPL US".into(),
            yellow_key: "Equity".into(), active: true,
            security: "AAPL US Equity".into(), views: vec![] }];
        let sheet = SheetData { has_id_column: true, view_columns: vec![],
            rows: vec![SheetRow { row_number: 2, instrument_id: Some(7),
                label: "Apple".into(), class: "Equity".into(),
                identifier: "MSFT US".into(), yellow_key: "Equity".into(),
                active: true, views: vec![] }] };
        let plan = diff(&sheet, &db, &["Equity".into()], &[], "hash");
        assert_eq!(plan.invalid_rows.len(), 1);
        assert!(plan.invalid_rows[0].reason.contains("identifier"));
        assert!(plan.edits.is_empty());
    }

    #[test]
    fn a_label_change_is_still_an_ordinary_edit() {
        let db = vec![DbInstrument { instrument_id: 7, label: "Apple".into(),
            class: "Equity".into(), identifier: "AAPL US".into(),
            yellow_key: "Equity".into(), active: true,
            security: "AAPL US Equity".into(), views: vec![] }];
        let sheet = SheetData { has_id_column: true, view_columns: vec![],
            rows: vec![SheetRow { row_number: 2, instrument_id: Some(7),
                label: "Apple Inc".into(), class: "Equity".into(),
                identifier: "AAPL US".into(), yellow_key: "Equity".into(),
                active: true, views: vec![] }] };
        let plan = diff(&sheet, &db, &["Equity".into()], &[], "hash");
        assert_eq!(plan.edits.len(), 1);
        assert!(plan.invalid_rows.is_empty());
    }

    #[test]
    fn flipping_active_is_a_retire_not_an_edit() {
        let db = vec![db_apple()];
        let mut r = row_from(&db[0]);
        r.active = false;
        let p = diff(&sheet(vec![r], true), &db, &classes(), &views(), "hash");
        assert!(p.edits.is_empty(), "active is its own category");
        assert_eq!(p.retires.len(), 1);
        assert_eq!(p.retires[0].id, 1);
    }

    #[test]
    fn flipping_active_back_on_is_a_reactivation() {
        let mut a = db_apple();
        a.active = false;
        let db = vec![a.clone()];
        let mut r = row_from(&a);
        r.active = true;
        let p = diff(&sheet(vec![r], true), &db, &classes(), &views(), "hash");
        assert_eq!(p.reactivations.len(), 1);
        assert!(p.retires.is_empty());
    }

    #[test]
    fn view_marks_become_membership_changes() {
        let db = vec![db_apple()];
        let mut r = row_from(&db[0]);
        r.views = vec!["Weekly".into()]; // was Daily
        let p = diff(&sheet(vec![r], true), &db, &classes(), &views(), "hash");
        assert_eq!(p.membership_changes.len(), 1);
        assert_eq!(p.membership_changes[0].added, vec!["Weekly".to_string()]);
        assert_eq!(p.membership_changes[0].removed, vec!["Daily".to_string()]);
    }

    #[test]
    fn a_missing_row_is_a_removal_when_the_sheet_has_an_id_column() {
        let db = vec![db_apple()];
        let p = diff(&sheet(vec![], true), &db, &classes(), &views(), "hash");
        assert_eq!(p.removals.len(), 1);
        assert_eq!(p.removals[0].id, 1);
    }

    /// Guardrail 1, spec §8.1 -- the one that makes pasted lists safe.
    #[test]
    fn a_sheet_without_an_id_column_never_proposes_a_removal() {
        let db = vec![db_apple()];
        let pasted = SheetRow {
            row_number: 2, instrument_id: None, label: "Microsoft".into(), class: "Equity".into(),
            identifier: "MSFT US".into(), yellow_key: "Equity".into(), active: true, views: vec![],
        };
        let p = diff(&sheet(vec![pasted], false), &db, &classes(), &views(), "hash");
        assert!(p.removals.is_empty(), "a pasted list must not delete the book");
        assert_eq!(p.adds.len(), 1);
    }

    /// Guardrail 2, spec §8.1.
    #[test]
    fn removing_more_than_half_the_active_book_demands_typed_confirmation() {
        let db: Vec<DbInstrument> = (1..=4).map(|i| {
            let mut a = db_apple();
            a.instrument_id = i;
            a.label = format!("A{i}");
            a.security = format!("A{i} US Equity");
            a.identifier = format!("A{i} US");
            a
        }).collect();
        let kept = SheetRow { row_number: 2, ..row_from(&db[0]) };
        let p = diff(&sheet(vec![kept], true), &db, &classes(), &views(), "hash");
        assert_eq!(p.removals.len(), 3);
        assert_eq!(p.active_asset_count, 4);
        assert!(p.requires_typed_confirmation);

        // Two of four is not "more than half".
        let two = vec![SheetRow { row_number: 2, ..row_from(&db[0]) },
                       SheetRow { row_number: 3, ..row_from(&db[1]) },
                       SheetRow { row_number: 4, ..row_from(&db[2]) }];
        let q = diff(&sheet(two, true), &db, &classes(), &views(), "hash");
        assert_eq!(q.removals.len(), 1);
        assert!(!q.requires_typed_confirmation);
    }

    #[test]
    fn unknown_class_unknown_id_and_bad_identifier_are_invalid_rows() {
        let db = vec![db_apple()];

        let mut bad_class = row_from(&db[0]);
        bad_class.class = "Nonexistent".into();
        assert_eq!(diff(&sheet(vec![bad_class], true), &db, &classes(), &views(), "h")
                       .invalid_rows.len(), 1);

        let mut bad_id = row_from(&db[0]);
        bad_id.instrument_id = Some(999);
        let p = diff(&sheet(vec![bad_id], true), &db, &classes(), &views(), "h");
        assert_eq!(p.invalid_rows.len(), 1);
        assert!(p.invalid_rows[0].reason.contains("999"));

        // An identifier that is only the yellow key never had a security in
        // it -- the single-column replacement for the old ticker/isin
        // mismatch case, which no longer exists now that id_kind is gone.
        let mut bad_identifier = row_from(&db[0]);
        bad_identifier.identifier = "Equity".into();
        assert_eq!(diff(&sheet(vec![bad_identifier], true), &db, &classes(), &views(), "h")
                       .invalid_rows.len(), 1);
    }

    #[test]
    fn a_duplicate_id_and_a_colliding_security_are_both_rejected() {
        let mut msft = db_apple();
        msft.instrument_id = 2;
        msft.label = "Microsoft".into();
        msft.identifier = "MSFT US".into();
        msft.security = "MSFT US Equity".into();
        let db = vec![db_apple(), msft];

        let dup = vec![row_from(&db[0]), SheetRow { row_number: 3, ..row_from(&db[0]) }];
        let p = diff(&sheet(dup, true), &db, &classes(), &views(), "h");
        assert!(p.invalid_rows.iter().any(|i| i.reason.contains("twice")),
                "got {:?}", p.invalid_rows);

        // Renaming Apple onto Microsoft's security must not reach the store.
        let mut collide = row_from(&db[0]);
        collide.identifier = "MSFT US".into();
        let q = diff(&sheet(vec![collide, SheetRow { row_number: 3, ..row_from(&db[1]) }], true),
                     &db, &classes(), &views(), "h");
        assert!(q.invalid_rows.iter().any(|i| i.reason.contains("MSFT US Equity")),
                "got {:?}", q.invalid_rows);
    }

    /// Finding I1: a view the DB knows about but the sheet carries no column
    /// for must be left alone entirely -- the sheet does not speak for it, so
    /// its absence from any row must never be read as "remove from this view".
    /// Deliberately does NOT use the `sheet()` fixture, which always carries
    /// the full view list and is exactly why the 16 pre-existing differ tests
    /// never caught this.
    #[test]
    fn a_view_missing_from_the_sheets_columns_is_left_alone() {
        let db = vec![db_apple()]; // db_apple belongs to "Daily"
        let mut r = row_from(&db[0]);
        r.views = vec![]; // the sheet has no "Daily" column, so nothing can be ticked
        let s = SheetData {
            has_id_column: true,
            view_columns: vec!["Weekly".into()], // "Daily" is not a column in this sheet
            rows: vec![r],
        };
        let p = diff(&s, &db, &classes(), &views(), "hash");
        assert!(p.membership_changes.is_empty(),
                "a view absent from the sheet's columns must not be touched, got {:?}",
                p.membership_changes);
    }

    /// Companion to the test above: a view that IS present in the sheet's
    /// columns must still behave exactly as before -- an unticked cell in a
    /// present column still means "remove from this view".
    #[test]
    fn a_view_present_in_the_sheets_columns_still_adds_and_removes() {
        let db = vec![db_apple()]; // belongs to "Daily"
        let mut r = row_from(&db[0]);
        r.views = vec!["Weekly".into()]; // ticked Weekly, left Daily unticked
        let s = SheetData {
            has_id_column: true,
            view_columns: vec!["Daily".into(), "Weekly".into()],
            rows: vec![r],
        };
        let p = diff(&s, &db, &classes(), &views(), "hash");
        assert_eq!(p.membership_changes.len(), 1);
        assert_eq!(p.membership_changes[0].added, vec!["Weekly".to_string()]);
        assert_eq!(p.membership_changes[0].removed, vec!["Daily".to_string()]);
    }

    #[test]
    fn a_view_column_naming_an_unknown_view_is_reported_against_the_header() {
        let db = vec![db_apple()];
        let s = SheetData {
            has_id_column: true,
            view_columns: vec!["Daily".into(), "Ghost".into()],
            rows: vec![row_from(&db[0])],
        };
        let p = diff(&s, &db, &classes(), &views(), "h");
        assert_eq!(p.invalid_rows.len(), 1);
        assert_eq!(p.invalid_rows[0].row_number, 1, "header problems belong to row 1");
        assert!(p.invalid_rows[0].reason.contains("Ghost"));
    }

    /// Regression: a class typo on a row carrying a real id must not also read
    /// as "the user deleted this instrument" -- exactly what guardrail 1 exists
    /// to prevent, and the one place the previous fix (Task 9's first pass) did
    /// not go far enough.
    #[test]
    fn an_invalid_class_on_an_existing_row_is_not_also_a_removal() {
        let db = vec![db_apple()];
        let mut bad_class = row_from(&db[0]);
        bad_class.class = "Nonexistent".into();
        let p = diff(&sheet(vec![bad_class], true), &db, &classes(), &views(), "h");
        assert_eq!(p.invalid_rows.len(), 1);
        assert!(p.removals.is_empty(),
                "a broken row must not also be proposed for deletion, got {:?}", p.removals);
    }

    /// Same shape, for the other validation that runs ahead of identity.
    #[test]
    fn an_empty_label_on_an_existing_row_is_not_also_a_removal() {
        let db = vec![db_apple()];
        let mut bad_label = row_from(&db[0]);
        bad_label.label = String::new();
        let p = diff(&sheet(vec![bad_label], true), &db, &classes(), &views(), "h");
        assert_eq!(p.invalid_rows.len(), 1);
        assert!(p.removals.is_empty(),
                "a broken row must not also be proposed for deletion, got {:?}", p.removals);
    }

    /// Regression: a rejected row's security claim must not survive to
    /// wrongly indict a later, entirely valid row that resolves to the same
    /// security -- the chained-claim bug in Task 9's first pass.
    #[test]
    fn a_rejected_claim_does_not_block_a_later_valid_row() {
        let mut msft = db_apple();
        msft.instrument_id = 2;
        msft.label = "Microsoft".into();
        msft.identifier = "MSFT US".into();
        msft.security = "MSFT US Equity".into();
        let db = vec![db_apple(), msft];

        // Row 2 renames Apple onto Microsoft's security and is rejected. Row 3
        // is Microsoft's own, completely unmodified row.
        let mut collide = row_from(&db[0]);
        collide.identifier = "MSFT US".into();
        let rows = vec![collide, SheetRow { row_number: 3, ..row_from(&db[1]) }];
        let p = diff(&sheet(rows, true), &db, &classes(), &views(), "h");

        assert_eq!(p.invalid_rows.len(), 1,
                   "only the renamed row should be invalid, got {:?}", p.invalid_rows);
        assert_eq!(p.invalid_rows[0].row_number, 2);
        assert_eq!(p.invalid_rows[0].reason,
                   "security 'MSFT US Equity' already belongs to instrument #2 'Microsoft'; \
                    if you just imported this sheet, export it again to pick up the new ids");
    }

    /// The collision and within-sheet-duplicate checks apply to add rows
    /// (blank id) too, not only to edits of an existing id.
    #[test]
    fn add_rows_are_checked_for_collisions_and_duplicates_too() {
        let db = vec![db_apple()];

        // An add that resolves to a security a DB instrument already owns.
        // Apple's own row is deliberately left out of the sheet so this
        // exercises the owner-collision check itself, not the
        // in-sheet claimed-by-row-N path covered by the next case.
        let mut add_collides = row_from(&db[0]);
        add_collides.instrument_id = None;
        add_collides.row_number = 2;
        add_collides.label = "Also Apple".into();
        // identifier/yellow_key untouched -> resolves to "AAPL US Equity", Apple's own.
        let p = diff(&sheet(vec![add_collides], true), &db, &classes(), &views(), "h");
        assert!(p.adds.is_empty(), "the colliding add must not be proposed, got {:?}", p.adds);
        assert_eq!(p.invalid_rows.len(), 1);
        assert_eq!(p.invalid_rows[0].row_number, 2);
        assert_eq!(p.invalid_rows[0].reason,
                   "security 'AAPL US Equity' already belongs to instrument #1 'Apple'; \
                    if you just imported this sheet, export it again to pick up the new ids");

        // Two adds in the same sheet claiming the same brand-new security.
        let mut add1 = row_from(&db[0]);
        add1.instrument_id = None; add1.row_number = 3; add1.label = "New1".into();
        add1.identifier = "NEW US".into();
        let mut add2 = row_from(&db[0]);
        add2.instrument_id = None; add2.row_number = 4; add2.label = "New2".into();
        add2.identifier = "NEW US".into();
        let q = diff(&sheet(vec![row_from(&db[0]), add1, add2], true), &db,
                     &classes(), &views(), "h");
        assert_eq!(q.adds.len(), 1, "only the first add should succeed, got {:?}", q.adds);
        assert_eq!(q.invalid_rows.len(), 1);
        assert_eq!(q.invalid_rows[0].row_number, 4);
        assert_eq!(q.invalid_rows[0].reason,
                   "security 'NEW US Equity' is already claimed by row 3");
    }
}
