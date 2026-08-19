<script lang="ts">
  import { api, type EstimateOut, type IssueRow, type RunRow, type View } from "./api";

  let views = $state<View[]>([]);
  let selectedViewId = $state<number | null>(null);
  let estimate = $state<EstimateOut | null>(null);
  let gaps = $state<[string, string][]>([]);
  let runs = $state<RunRow[]>([]);
  let selectedRun = $state<RunRow | null>(null);
  let issues = $state<IssueRow[]>([]);
  let error = $state("");
  // Prevents a double-click from starting two concurrent pipelines (two Excel
  // instances hitting the same pending path, double Bloomberg hits) while a
  // multi-minute run/backfill await is in flight.
  let inFlight = $state(false);

  type PendingConfirm =
    | { kind: "eod"; estimated: number; today_total: number }
    | { kind: "backfill"; start: string; end: string; estimated: number; today_total: number };
  let pending = $state<PendingConfirm | null>(null);

  async function loadViews() {
    try {
      views = await api.listViews();
      if (views.length && selectedViewId === null) selectedViewId = views[0].id;
    } catch (e) { error = String(e); }
  }

  async function loadViewData() {
    if (selectedViewId === null) return;
    try {
      estimate = await api.estimateView(selectedViewId);
      gaps = await api.detectViewGaps(selectedViewId);
    } catch (e) { error = String(e); }
  }

  async function refreshRuns() {
    try { runs = await api.listRuns(50); }
    catch (e) { error = String(e); }
  }

  $effect(() => { loadViews(); });
  $effect(() => { selectedViewId; loadViewData(); pending = null; });
  $effect(() => {
    refreshRuns();
    const iv = setInterval(refreshRuns, 5000);
    return () => clearInterval(iv);
  });

  async function runNow() {
    if (selectedViewId === null) return;
    inFlight = true;
    try {
      const outcome = await api.runEodNow(selectedViewId, false);
      if ("NeedsConfirmation" in outcome) {
        pending = { kind: "eod", ...outcome.NeedsConfirmation };
      } else {
        pending = null;
        await Promise.all([loadViewData(), refreshRuns()]);
      }
    } catch (e) { error = String(e); }
    finally { inFlight = false; }
  }

  async function backfillRange(start: string, end: string) {
    if (selectedViewId === null) return;
    inFlight = true;
    try {
      const outcome = await api.runBackfillNow(selectedViewId, start, end, false);
      if ("NeedsConfirmation" in outcome) {
        pending = { kind: "backfill", start, end, ...outcome.NeedsConfirmation };
      } else {
        pending = null;
        await Promise.all([loadViewData(), refreshRuns()]);
      }
    } catch (e) { error = String(e); }
    finally { inFlight = false; }
  }

  async function confirmPending() {
    if (selectedViewId === null || pending === null) return;
    inFlight = true;
    try {
      if (pending.kind === "eod") {
        await api.runEodNow(selectedViewId, true);
      } else {
        await api.runBackfillNow(selectedViewId, pending.start, pending.end, true);
      }
      pending = null;
      await Promise.all([loadViewData(), refreshRuns()]);
    } catch (e) { error = String(e); }
    finally { inFlight = false; }
  }

  async function selectRun(run: RunRow) {
    selectedRun = run;
    try { issues = await api.listIssues(run.id); }
    catch (e) { error = String(e); }
  }
</script>

{#if error}<p class="error">{error}</p>{/if}
<section>
  <h2>Run</h2>
  <select bind:value={selectedViewId}>
    {#each views as v}<option value={v.id}>{v.name}</option>{/each}
  </select>

  {#if estimate}
    <p class:amber={estimate.level === "SoftWarn"} class:red={estimate.level === "HardConfirm"}>
      ~{estimate.estimated} hits (today so far: {estimate.today_total})
    </p>
  {/if}

  <button onclick={runNow} disabled={inFlight || selectedViewId === null}>Run now</button>

  {#if pending}
    <div class="confirm">
      <p>
        This {pending.kind === "backfill" ? "backfill" : "run"} is estimated at
        {pending.estimated} hits (today so far: {pending.today_total}). Confirm to proceed.
      </p>
      <button onclick={confirmPending} disabled={inFlight}>Confirm run</button>
      <button onclick={() => (pending = null)}>Cancel</button>
    </div>
  {/if}

  <h2>Gaps (last 30 days)</h2>
  <table>
    <thead><tr><th>Start</th><th>End</th><th></th></tr></thead>
    <tbody>
      {#each gaps as [start, end]}
        <tr>
          <td>{start}</td><td>{end}</td>
          <td><button onclick={() => backfillRange(start, end)} disabled={inFlight}>Backfill</button></td>
        </tr>
      {/each}
    </tbody>
  </table>

  <h2>Run history</h2>
  <table>
    <thead>
      <tr><th>ID</th><th>Kind</th><th>Trigger</th><th>Status</th><th>Started</th><th>Finished</th><th>Est. hits</th></tr>
    </thead>
    <tbody>
      {#each runs as r}
        <tr class:selected={selectedRun?.id === r.id} onclick={() => selectRun(r)}>
          <td>{r.id}</td><td>{r.kind}</td><td>{r.trigger_kind}</td>
          <td>
            {r.status}
            {#if r.status === "failed" && r.error_summary}
              <br /><span class="error-detail">{r.error_summary}</span>
            {/if}
          </td>
          <td>{r.started_at}</td><td>{r.finished_at ?? ""}</td><td>{r.estimated_hits}</td>
        </tr>
      {/each}
    </tbody>
  </table>

  {#if selectedRun}
    <h2>Issues — run #{selectedRun.id}</h2>
    <table>
      <thead><tr><th>Severity</th><th>Code</th><th>Detail</th><th>Instrument</th><th>Field</th><th>Obs. date</th></tr></thead>
      <tbody>
        {#each issues as i}
          <tr>
            <td>{i.severity}</td><td>{i.code}</td><td>{i.detail}</td>
            <td>{i.instrument_id ?? ""}</td><td>{i.field_id ?? ""}</td><td>{i.obs_date ?? ""}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>

<style>
  .error { color: #c00; }
  .error-detail { color: #c00; font-size: 0.85em; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; width: 100%; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  tr.selected { background: #eef; }
  tbody tr { cursor: pointer; }
  .amber { color: #b8860b; font-weight: bold; }
  .red { color: #c00; font-weight: bold; }
  .confirm { border: 1px solid #c00; padding: 0.5rem; margin-top: 0.5rem; }
</style>
