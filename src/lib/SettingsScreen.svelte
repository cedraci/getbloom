<script lang="ts">
  import { api, type AppConfig, type ScheduleRow, type View } from "./api";

  let cfg = $state<AppConfig>({ data_dir: "", soft_limit: 0, refresh_timeout_s: 0 });
  let schedules = $state<ScheduleRow[]>([]);
  let views = $state<View[]>([]);
  let error = $state("");

  let newSchedule = $state({ view_id: 0, window_start: "09:00", window_end: "18:00", active: true });

  async function reload() {
    try {
      cfg = await api.getSettings();
      schedules = await api.listSchedules();
      views = await api.listViews();
      if (views.length && !newSchedule.view_id) newSchedule.view_id = views[0].id;
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });

  async function saveConfig() {
    try { await api.saveSettings(cfg); await reload(); }
    catch (e) { error = String(e); }
  }

  async function upsert() {
    try {
      await api.upsertSchedule(newSchedule.view_id, newSchedule.window_start, newSchedule.window_end, newSchedule.active);
      await reload();
    } catch (e) { error = String(e); }
  }

  async function toggleScheduleActive(s: ScheduleRow) {
    try {
      await api.upsertSchedule(s.view_id, s.window_start, s.window_end, !s.active);
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
      Refresh timeout (s)
      <input type="number" bind:value={cfg.refresh_timeout_s} min="0" required />
    </label>
    <button type="submit">Save settings</button>
  </form>

  <h2>Schedules</h2>
  <table>
    <thead>
      <tr><th>View</th><th>Window start</th><th>Window end</th><th>Drawn at</th><th>Last result</th><th>Active</th></tr>
    </thead>
    <tbody>
      {#each schedules as s}
        <tr>
          <td>{viewName(s.view_id)}</td>
          <td>{s.window_start}</td>
          <td>{s.window_end}</td>
          <td>{s.drawn_at ?? ""}</td>
          <td>{s.last_result ?? ""}</td>
          <td><input type="checkbox" checked={s.active} onchange={() => toggleScheduleActive(s)} /></td>
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
    <label><input type="checkbox" bind:checked={newSchedule.active} /> Active</label>
    <button type="submit">Save schedule</button>
  </form>
</section>

<style>
  .error { color: #c00; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  h3 { margin-top: 1rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  form { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; margin-top: 0.5rem; }
  form label { display: flex; flex-direction: column; font-size: 0.85em; gap: 0.15rem; }
</style>
