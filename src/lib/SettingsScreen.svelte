<script lang="ts">
  import { api, type AppConfig, type AssetClass, type EntityKind, type ScheduleRow, type View } from "./api";
  import DeleteDialog from "./DeleteDialog.svelte";

  let cfg = $state<AppConfig>({ data_dir: "", soft_limit: 0, request_timeout_s: 0, python_path: "" });
  let schedules = $state<ScheduleRow[]>([]);
  let views = $state<View[]>([]);
  let assetClasses = $state<AssetClass[]>([]);
  // Per-row draft so a checkbox/select/number edit doesn't write until Save
  // is clicked -- qc_stale_days_default is kept as a string so an emptied
  // input means "off" rather than the CHECK-tripping 0 (same convention as
  // ViewsScreen's field quality-gate inputs).
  let classEdits = $state<Record<number, {
    corp_actions_capable: boolean; ma_capable: boolean;
    adjustment_style: string; qc_stale_days_default: string;
  }>>({});
  let error = $state("");
  let pending = $state<{ kind: EntityKind; id: number } | null>(null);

  function afterDelete(changed: boolean) {
    pending = null;
    if (changed) reload();
  }

  let newSchedule = $state({ view_id: 0, window_start: "09:00", window_end: "18:00", active: true,
                             verify_dow: 5 as number | null });

  async function reload() {
    try {
      cfg = await api.getSettings();
      schedules = await api.listSchedules();
      views = await api.listViews();
      if (views.length && !newSchedule.view_id) newSchedule.view_id = views[0].id;
      assetClasses = await api.listAssetClasses();
      classEdits = Object.fromEntries(assetClasses.map((c) => [c.id, {
        corp_actions_capable: c.corp_actions_capable,
        ma_capable: c.ma_capable,
        adjustment_style: c.adjustment_style,
        qc_stale_days_default: c.qc_stale_days_default === null ? "" : String(c.qc_stale_days_default),
      }]));
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });

  // Svelte 5 binds an emptied type="number" input to null, not "" -- coerce
  // both the same way (blank/cleared -> off) rather than letting Number(null)
  // silently become 0 and trip the DB's CHECK constraint as a raw error.
  function toOptionalNumber(v: unknown): number | null {
    return v === null || v === undefined || v === "" ? null : Number(v);
  }

  async function saveCapabilities(id: number) {
    const e = classEdits[id];
    if (!e) return;
    try {
      await api.updateAssetClassCapabilities(id, e.corp_actions_capable, e.ma_capable,
        e.adjustment_style, toOptionalNumber(e.qc_stale_days_default));
      await reload();
    } catch (err) { error = String(err); }
  }

  async function saveConfig() {
    try { await api.saveSettings(cfg); await reload(); }
    catch (e) { error = String(e); }
  }

  async function upsert() {
    try {
      await api.upsertSchedule(newSchedule.view_id, newSchedule.window_start, newSchedule.window_end,
                               newSchedule.active, newSchedule.verify_dow);
      await reload();
    } catch (e) { error = String(e); }
  }

  async function toggleScheduleActive(s: ScheduleRow) {
    try {
      await api.upsertSchedule(s.view_id, s.window_start, s.window_end, !s.active, s.verify_dow);
      await reload();
    } catch (e) { error = String(e); }
  }

  function viewName(id: number) {
    return views.find((v) => v.id === id)?.name ?? String(id);
  }
</script>

{#if error}<p class="error">{error}</p>{/if}
<section>
  <h2>Configuration</h2>
  <form onsubmit={(e) => { e.preventDefault(); saveConfig(); }}>
    <label>
      Data dir
      <input bind:value={cfg.data_dir} placeholder="C:\bloomdata" required />
    </label>
    <label>
      Soft limit
      <input type="number" bind:value={cfg.soft_limit} min="0" required />
    </label>
    <label>
      Request timeout (s)
      <input type="number" bind:value={cfg.request_timeout_s} min="0" required />
    </label>
    <label>
      Python (BLPAPI sidecar)
      <input bind:value={cfg.python_path} placeholder="python" required />
    </label>
    <button type="submit">Save settings</button>
  </form>

  <h2>Schedules</h2>
  <table>
    <thead>
      <tr><th>View</th><th>Window start</th><th>Window end</th><th>Verify</th><th>Drawn at</th><th>Last result</th><th>Active</th><th></th></tr>
    </thead>
    <tbody>
      {#each schedules as s}
        <tr>
          <td>{viewName(s.view_id)}</td>
          <td>{s.window_start}</td>
          <td>{s.window_end}</td>
          <td>{s.verify_dow ? ["","Mon","Tue","Wed","Thu","Fri","Sat","Sun"][s.verify_dow] + (s.last_verified_on ? ` (last ${s.last_verified_on})` : "") : "off"}</td>
          <td>{s.drawn_at ?? ""}</td>
          <td>{s.last_result ?? ""}</td>
          <td><input type="checkbox" checked={s.active} onchange={() => toggleScheduleActive(s)} /></td>
          <td><button class="x" title="Remove schedule"
                      onclick={() => (pending = { kind: "schedule", id: s.id })}>&times;</button></td>
        </tr>
      {/each}
    </tbody>
  </table>

  <h3>Add / update schedule</h3>
  <form onsubmit={(e) => { e.preventDefault(); upsert(); }}>
    <select bind:value={newSchedule.view_id}>
      {#each views as v}<option value={v.id}>{v.name}</option>{/each}
    </select>
    <input type="time" bind:value={newSchedule.window_start} required />
    <input type="time" bind:value={newSchedule.window_end} required />
    <label>
      Verify day
      <select bind:value={newSchedule.verify_dow}
              title="Once a week, re-fetch the trailing 5 weekdays so upstream restatements are caught. Off = never.">
        <option value={null}>Off</option>
        <option value={1}>Monday</option>
        <option value={2}>Tuesday</option>
        <option value={3}>Wednesday</option>
        <option value={4}>Thursday</option>
        <option value={5}>Friday</option>
      </select>
    </label>
    <label><input type="checkbox" bind:checked={newSchedule.active} /> Active</label>
    <button type="submit">Save schedule</button>
  </form>

  <h2>Asset classes</h2>
  <table>
    <thead>
      <tr><th>Name</th><th>Corp actions</th><th>M&amp;A lifecycle</th><th>Adjustment style</th><th>Stale after (days)</th><th></th></tr>
    </thead>
    <tbody>
      {#each assetClasses as c}
        {@const e = classEdits[c.id]}
        {#if e}
          <tr>
            <td>{c.name}</td>
            <td><input type="checkbox" bind:checked={e.corp_actions_capable} /></td>
            <td><input type="checkbox" bind:checked={e.ma_capable} /></td>
            <td>
              <select bind:value={e.adjustment_style}>
                <option value="factors">factors</option>
                <option value="none">none</option>
              </select>
            </td>
            <td><input type="number" bind:value={e.qc_stale_days_default} min="2" placeholder="off" /></td>
            <td><button onclick={() => saveCapabilities(c.id)}>Save</button></td>
          </tr>
        {/if}
      {/each}
    </tbody>
  </table>
</section>

{#if pending}
  <DeleteDialog kind={pending.kind} id={pending.id} onclose={afterDelete} />
{/if}

<style>
  .error { color: #c00; }
  .x { border: none; background: none; color: #c00; cursor: pointer;
       font-size: 1rem; line-height: 1; padding: 0 0.3rem; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  h3 { margin-top: 1rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  form { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; margin-top: 0.5rem; }
  form label { display: flex; flex-direction: column; font-size: 0.85em; gap: 0.15rem; }
</style>
