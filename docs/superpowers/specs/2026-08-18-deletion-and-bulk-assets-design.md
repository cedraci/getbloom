# Deletion and bulk asset management — design

**Date:** 2026-08-18
**Status:** approved for planning
**Extends:** the registry surface in `2026-08-13-bloomberg-eod-pipeline-design.md` §3
and the BLPAPI redesign in `2026-08-18-blpapi-redesign.md`, neither of which
addressed removal.

## 1. Problem

The UI can create asset classes, assets, fields, views and schedules. It can
delete none of them. The only removal of any kind is `set_asset_active`, a soft
toggle on assets alone. Fields, views, classes and schedules accumulate forever,
including rows created by mistake while testing — the broken
`AAPL US Equity Equity` asset that migration 0004 had to deactivate is still in
the table because nothing can remove it.

Separately, assets are added one at a time through a form. Growing the book from
three names to a realistic size, and choosing which of them each run collects
data for, is impractical at that rate.

## 2. Scope

In scope:

- Removal for every entity the UI can create.
- Export of the asset registry, with view membership, to a `.xlsx` file.
- Import of that file, diffed against the database and confirmed before applying.

Out of scope, deliberately:

- A fields sheet. The driver for the bulk path was scaling the *asset* list;
  fields stay in the UI. A second worksheet is additive and can follow later.
- Undo. Retire is already reversible by flipping `active`; Purge is not, and is
  gated behind an explicit confirmation rather than an undo log.
- Editing observations. This design never rewrites collected data, only removes
  it wholesale with the entity it belongs to.

## 3. Decisions and rationale

1. **Deletion asks per deletion.** There is no global "always purge" or "always
   retire" setting. The confirm dialog reports what is attached and offers the
   choices that entity supports. The two motivations — clearing test mistakes and
   retiring names no longer tracked — want opposite defaults, so the choice
   belongs at the point of use.

2. **Rules fit the entity, not one uniform dialog.** A schedule holds no history
   and does not deserve a two-option dialog; a view with 25 runs behind it cannot
   honestly be purged at all. See §5.

3. **Purge is an explicit transactional delete; foreign keys stay restrictive.**
   No `ON DELETE CASCADE` is added. Cascades are invisible at the call site and
   silently widen the blast radius of any future delete, whereas explicit
   statements fail loudly when a new referencing table appears. This follows the
   conservatism of migration 0004, which was written conditional precisely
   because migrations run at startup.

4. **`run` and `hit_ledger` are never touched by a purge.** A run is per-view and
   spans many assets, and the hit ledger is the record of what was spent against
   the Bloomberg budget. Both must stay truthful after the entity they mention is
   gone. A purged asset therefore leaves runs that reference work done on it —
   correct, because that work did happen.

5. **The bulk sheet is registry plus one column per view.** One file controls both
   what exists and what each run collects, which is what "mass update the assets
   the tool saves data for" actually asks for.

6. **A row missing from an imported sheet is a proposed removal**, surfaced in the
   diff with the same Retire/Purge choice — but only when the file is an export
   (see §8.1). This keeps the round trip honest without making a pasted list
   dangerous.

7. **Format is `.xlsx`, via `rust_xlsxwriter` and `calamine`.** Both are pure
   Rust, need no Excel installation, and were proven in this codebase before the
   BLPAPI switch removed them. CSV was rejected: this machine is French-locale,
   where Excel writes and expects `;` rather than `,` and will open a
   comma-separated file into a single column. `.xlsx` also carries a frozen
   header, dropdown validation and a hidden `id` column, none of which CSV can.

## 4. Schema impact: none

No migration is required.

`asset`, `field_def`, `view` and `schedule` already carry `active`, and
`views.rs` already filters view membership on `a.active` (line 77) and `f.active`
(lines 88 and 101). Retiring an entity therefore stops collection through the
existing code path, with no change to the fetch pipeline.

`asset_class` has no `active` column and needs none: its rule is
delete-only-when-empty, so there is no retired state to represent.

**One behaviour to confirm during implementation:** retiring a view must stop its
scheduled runs. The scheduler selects due schedules from `schedule`; that query
must also require `view.active`. If it does not today, adding the filter is part
of this work.

## 5. Deletion semantics per entity

| Entity | Retire | Purge | Blocked when |
|---|---|---|---|
| Schedule | — | plain delete | never |
| Asset | `active = false` | see below | never |
| Field | `active = false` | see below | never |
| View | `active = false` | allowed only if it has no runs | purge, when `runs > 0` |
| Asset class | — | plain delete | any asset or field still references it |

Purge statement order, all inside one transaction:

- **Asset:** `ingest_issue` (by `asset_id`) → `observation` (by `asset_id`) →
  `view_asset` → `asset`.
- **Field:** `ingest_issue` (by `field_id`) → `observation` (by `field_id`) →
  `view_field` → `field_def`.
- **View, no runs:** `schedule` (by `view_id`) → `view`. `view_asset` and
  `view_field` cascade from `view` already.
- **Asset class:** the row alone, after the emptiness check.

A blocked deletion returns `AppError::DeleteBlocked { reason, counts }` — a
structured value naming what stands in the way, not a raw Postgres foreign-key
string.

## 6. Command surface

```
describe_deletion(kind: EntityKind, id: i64) -> DeletionImpact
delete_asset(id: i64, mode: DeleteMode)          // Retire | Purge
delete_field(id: i64, mode: DeleteMode)
delete_view(id: i64, mode: DeleteMode)
delete_asset_class(id: i64)
delete_schedule(id: i64)

export_assets_xlsx(path: String)
preview_assets_import(path: String) -> ImportPlan
apply_assets_import(path: String, file_hash: String,
                    removal_modes: Vec<(i64, DeleteMode)>) -> ImportResult
```

`DeletionImpact` carries `observations`, `first_obs`, `last_obs`, `views`,
`issues`, `runs` and `children`, so the dialog states counts that came from the
database rather than an optimistic guess.

The UI calls `describe_deletion` to render the dialog, but every `delete_*`
command re-checks its own invariants server-side. The dialog is a courtesy; the
command is the enforcement.

## 7. Sheet contract

One worksheet named `Assets`:

| Column | Role on import |
|---|---|
| `id` | Read-only. Blank means a new asset. Present means edit the row with that id. |
| `label` | Editable |
| `class` | Editable, must name an existing asset class |
| `id_kind` | Editable, `ticker` or `isin` |
| `ticker` | Editable, required when `id_kind` is `ticker` |
| `isin` | Editable, required when `id_kind` is `isin` |
| `yellow_key` | Editable |
| `active` | Editable, `yes` or `no` |
| `security` | Read-only. The derived `bdp_security`. |
| one per view, header is the view name | `x` for member, empty for not |

The export freezes the header row and attaches dropdown validation to `class`,
`id_kind`, `yellow_key` and `active`.

`security` exists so the string that will actually reach Bloomberg is visible
while editing — the doubled-yellow-key fault fixed in `b8273e0` was invisible
precisely because it lived only in a derived column. On import the value is
ignored and `bdp_security` is recomputed through `registry::resolve_bdp_security`,
so the sheet cannot reintroduce a malformed security.

Renaming an asset is an edit, not a delete-and-add, because identity travels in
the `id` column rather than in the ticker.

## 8. Import pipeline

Two phases:

1. `preview_assets_import(path)` parses the sheet, diffs it against the database,
   and returns an `ImportPlan` plus the SHA-256 of the file bytes.
2. `apply_assets_import(path, file_hash, removal_modes)` re-reads the file,
   re-diffs, and refuses if the hash no longer matches. A plan you reviewed can
   never be applied against a file that changed underneath it.

`ImportPlan` groups changes as `adds`, `edits` (with the changed columns named),
`retires`, `reactivations`, `membership_changes`, `removals` and `invalid_rows`.

Application order inside one transaction: validate everything, then adds, edits,
membership changes, retires, and purges. All rows land or none do.

### 8.1 Guardrails

1. **A sheet with no `id` column can never propose a removal.** Such a file is
   treated as add-and-edit only. Hand-built and pasted lists are therefore safe
   by construction, and only a file produced by Export can remove anything. This
   is the guardrail that does the real work.
2. **Removals affecting more than half the active assets require typing the
   removal count** to confirm.
3. **Each removal carries its own Retire-or-Purge choice** in the diff screen,
   defaulting to Retire.
4. **Apply refuses any removal the caller did not review.** Every id in the
   fresh plan's removals must carry an explicit mode. Without this, an asset
   created by another writer between preview and apply -- absent from the
   sheet, therefore a removal, and never something the user looked at -- would
   fall back to Retire silently.

### 8.2 The workbook is rewritten after a successful apply

A blank-id row is an add, and the id the database assigns it exists nowhere in
the file. If the file were left as the user saved it, the next preview would
read that newly created asset as *both* an invalid duplicate claim on its own
security *and* a removal, because its id never appears in the sheet. So a
successful apply rewrites the workbook over the same path with the committed
state, and the new ids land in the file.

Two consequences, both of which the UI must handle:

- **The rewrite is best-effort and never fails the import.** The file is
  frequently locked -- the user's own copy open in Excel is the ordinary case
  on Windows -- and by the time the rewrite runs the transaction has already
  committed. `ImportResult.workbook_refreshed` reports it. False means the
  import succeeded and only the file is stale; it must be surfaced as a notice
  telling the user to close Excel and export again, never as a failed import.
  Telling a user their import failed when it did not leads them to "fix" the
  sheet by deleting the row they just added, which then proposes deleting the
  asset they just created.
- **A copy already open in Excel is now stale even on success.** The file on
  disk has been replaced; the window the user is looking at has not. One
  Ctrl-S from that window restores the blank-id row and puts them back in the
  case above. The import screen should tell the user to close and reopen the
  workbook after a successful import.

Neither state is destructive: the invalid-row message names the owning asset
and says to export again, and guardrail 4 means the orphaned asset can only be
removed by someone explicitly choosing to.

## 9. Error handling

Row-level validation collects, per spreadsheet row number: an unknown class, an
`id_kind` that does not match the populated identifier column, a rename that
would collide with `UNIQUE (bdp_security)`, an `id` that is not in the database,
and a view column naming a view that no longer exists.

Any invalid row blocks the entire import. Nothing is applied partially; the user
fixes the sheet and re-previews. This is stricter than skipping bad rows, and
deliberately so — a partially applied import leaves the sheet and the database
disagreeing, which is the state this feature exists to prevent.

## 10. UI changes

- `AssetsScreen.svelte`: a delete control per asset, per field and per class;
  Export and Import buttons.
- `ViewsScreen.svelte`: a delete control per view and per schedule.
- New `DeleteDialog.svelte`: renders `DeletionImpact` and offers the modes that
  entity supports, or explains why deletion is blocked.
- New `ImportDiff.svelte`: the grouped plan, a Retire/Purge selector on each
  removal, the typed confirmation when guardrail 2 trips, and Apply.

## 11. Testing

- **Differ as a pure function.** `(sheet rows, db rows) -> ImportPlan` takes no
  database and no file, so the interesting logic is unit-testable: adds, edits,
  renames via `id`, membership flips, removals, and every validation error.
- **Round trip.** Export then immediately import must produce an empty plan.
- **Guardrails.** A sheet without `id` proposes no removals; an over-threshold
  removal set demands the typed count; a stale hash is refused.
- **Deletion against Postgres**, in `db_integration.rs`: retire hides an asset
  from view resolution while its observations remain; purge removes observations
  and issues while leaving `run` and `hit_ledger` intact; a view with runs
  refuses to purge; a non-empty class refuses to delete.

Note for whoever runs these: every test in `db_integration.rs` is `#[ignore]`, so
`-- --ignored` is mandatory, and `BLOOM_TEST_DATABASE_URL` is set at User scope,
which an already-running shell does not inherit.

## 12. Deferred

- A fields worksheet in the same workbook.
- Bulk editing of asset classes and views through the sheet; both are created in
  the UI and referenced by name from the sheet.
- Any undo beyond re-adding a purged row by hand.
