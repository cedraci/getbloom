<script lang="ts">
  import { api, type DeleteMode, type ImportPlan, type ImportResult } from "./api";

  let { path, plan, onclose }: {
    path: string; plan: ImportPlan; onclose: (applied: boolean, notice?: string) => void;
  } = $props();

  // Retire is the default: the reversible option should never require a click.
  // Seeded once per plan by construction: AssetsScreen wraps this component in
  // {#key plan}, so a new plan always mounts a fresh ImportDiff instance and
  // this initializer reruns -- it is never left holding modes for ids that
  // belonged to a plan that has since been replaced.
  let modes = $state<Record<number, DeleteMode>>(
    Object.fromEntries(plan.removals.map((r) => [r.id, "retire" as DeleteMode])));
  let typed = $state("");
  let error = $state("");
  let busy = $state(false);

  const blocked = $derived(plan.invalid_rows.length > 0);
  const needsCount = $derived(plan.requires_typed_confirmation);
  const countOk = $derived(!needsCount || typed.trim() === String(plan.removals.length));
  const nothingToDo = $derived(
    plan.adds.length === 0 && plan.edits.length === 0 && plan.removals.length === 0 &&
    plan.retires.length === 0 && plan.reactivations.length === 0 &&
    plan.membership_changes.length === 0);

  // Renders the six ImportResult counts as a compact, comma-joined clause,
  // omitting zero terms -- so an import that only added two rows reads as
  // "2 added", not a wall of "0 edited, 0 retired, ...".
  function summarizeCounts(res: ImportResult): string {
    const parts: string[] = [];
    if (res.added) parts.push(`${res.added} added`);
    if (res.edited) parts.push(`${res.edited} edited`);
    if (res.retired) parts.push(`${res.retired} retired`);
    if (res.reactivated) parts.push(`${res.reactivated} reactivated`);
    if (res.membership_assets_updated) parts.push(`${res.membership_assets_updated} membership update(s)`);
    if (res.removed) parts.push(`${res.removed} removed`);
    return parts.join(", ");
  }

  async function apply() {
    busy = true; error = "";
    try {
      // Every id in plan.removals must carry a mode -- apply_assets_import refuses
      // any removal the caller did not review, so this cannot be trimmed down to
      // only the entries the user changed away from Retire.
      const pairs = plan.removals.map((r) => [r.id, modes[r.id]] as [number, DeleteMode]);
      const res = await api.applyAssetsImport(path, plan.file_hash, pairs,
                                              needsCount ? plan.removals.length : null);
      const counts = summarizeCounts(res);
      const summary = counts ? `Imported (${counts}).` : "Imported.";
      if (!res.workbook_refreshed) {
        // The import COMMITTED. Only the write-back of the new ids into the
        // workbook failed, and the file is almost always locked because the
        // user still has it open in Excel. Say what happened and what to do,
        // and never phrase this as a failed import.
        onclose(true, `${summary} The workbook could not be updated with the new `
                    + "ids -- close it in Excel and export again before your next "
                    + "import, or the added rows will look like deletions.");
        return;
      }
      // The rewrite succeeded, so the file on disk now carries the committed
      // ids -- but the copy in the user's own Excel window, if still open, does
      // not. One Ctrl-S from that stale window would restore the blank-id row.
      onclose(true, `${summary} The workbook was updated with the new ids -- `
                  + "close and reopen it in Excel before editing it again.");
    } catch (e) { error = String(e); busy = false; }
  }
</script>

<div class="backdrop">
  <div class="dialog">
    <h3>Import preview</h3>

    {#if !plan.has_id_column}
      <p class="warn">
        This sheet has no <code>id</code> column, so nothing can be removed by
        importing it &mdash; only additions and edits are possible. Export the
        workbook and edit that copy if you want to propose removals.
      </p>
    {/if}

    {#if blocked}
      <p class="error">
        {plan.invalid_rows.length} row(s) must be fixed before anything can be applied.
        Nothing is imported partially.
      </p>
      <ul>
        {#each plan.invalid_rows as r}<li>Row {r.row_number}: {r.reason}</li>{/each}
      </ul>
    {:else if nothingToDo}
      <p>The sheet matches the database. Nothing to do.</p>
    {:else}
      {#if plan.adds.length}
        <h4>Add ({plan.adds.length})</h4>
        <ul>{#each plan.adds as a}<li>{a.label} &mdash; {a.security}</li>{/each}</ul>
      {/if}
      {#if plan.edits.length}
        <h4>Edit ({plan.edits.length})</h4>
        <ul>{#each plan.edits as e}
          <li>{e.label} &mdash; {e.changed.join(", ")} &rarr; {e.security}</li>
        {/each}</ul>
      {/if}
      {#if plan.membership_changes.length}
        <h4>View membership ({plan.membership_changes.length})</h4>
        <ul>{#each plan.membership_changes as m}
          <li>{m.label}
            {#if m.added.length}&nbsp;+{m.added.join(", ")}{/if}
            {#if m.removed.length}&nbsp;&minus;{m.removed.join(", ")}{/if}
          </li>
        {/each}</ul>
      {/if}
      {#if plan.retires.length}
        <h4>Retire ({plan.retires.length})</h4>
        <ul>{#each plan.retires as r}<li>{r.label}</li>{/each}</ul>
      {/if}
      {#if plan.reactivations.length}
        <h4>Reactivate ({plan.reactivations.length})</h4>
        <ul>{#each plan.reactivations as r}<li>{r.label}</li>{/each}</ul>
      {/if}
      {#if plan.removals.length}
        <h4>Missing from the sheet ({plan.removals.length})</h4>
        <table>
          <thead><tr><th>Label</th><th>Security</th><th>Mode</th></tr></thead>
          <tbody>
            {#each plan.removals as r}
              <tr>
                <td>{r.label}</td><td class="mono">{r.security}</td>
                <td>
                  <select bind:value={modes[r.id]}>
                    <option value="retire">Retire</option>
                    <option value="purge">Purge</option>
                  </select>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
      {#if needsCount}
        <p class="warn">
          This removes {plan.removals.length} of {plan.active_asset_count} active assets.
          Type <strong>{plan.removals.length}</strong> to confirm.
        </p>
        <input bind:value={typed} placeholder="removal count" />
      {/if}
    {/if}

    <div class="actions">
      {#if error}<p class="error">{error}</p>{/if}
      {#if !blocked && !nothingToDo}
        <button onclick={apply} disabled={busy || !countOk}>Apply</button>
      {/if}
      <button onclick={() => onclose(false)} disabled={busy}>
        {blocked || nothingToDo ? "Close" : "Cancel"}
      </button>
    </div>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.35);
              display: flex; align-items: center; justify-content: center; }
  .dialog { background: #fff; border-radius: 4px; padding: 1.2rem;
            max-width: 46rem; max-height: 80vh; overflow-y: auto;
            box-shadow: 0 4px 20px rgba(0,0,0,0.3); }
  h3 { margin: 0 0 0.6rem; }
  h4 { margin: 0.9rem 0 0.2rem; }
  ul { margin: 0; padding-left: 1.2rem; color: #444; }
  .error { color: #c00; }
  .warn { color: #a60; }
  .mono { font-family: monospace; }
  table { border-collapse: collapse; margin-top: 0.3rem; }
  td, th { border: 1px solid #ccc; padding: 0.2rem 0.5rem; text-align: left; }
  .actions { display: flex; gap: 0.5rem; flex-wrap: wrap; align-items: center; margin-top: 1rem; }
  .actions .error { flex-basis: 100%; margin: 0 0 0.3rem; }
</style>
