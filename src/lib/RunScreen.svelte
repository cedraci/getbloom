<script lang="ts">
  import { api, type EstimateOut, type GapRow, type IssueRow,
           type LifecycleSummary, type RunOutcome,
           type RunRow, type View } from "./api";

  let views = $state<View[]>([]);
  let selectedViewId = $state<number | null>(null);
  let estimate = $state<EstimateOut | null>(null);
  let gaps = $state<GapRow[]>([]);
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
    | { kind: "backfill"; start: string; end: string; instrument_ids: number[] | null;
        estimated: number; today_total: number };
  let pending = $state<PendingConfirm | null>(null);

  // Corporate actions ride with every run; the completed outcome carries
  // their summary and it deserves its own visible line.
  let caLine = $state("");
  // P7: quality-gate findings this run produced (severity 'quality' issues).
  let qualityLine = $state("");
  function noteCorpActions(outcome: RunOutcome) {
    const ca = "Completed" in outcome ? outcome.Completed.corp_actions : null;
    caLine = ca
      ? `Corporate actions: ${ca.inserted} new, ${ca.amended} amended, `
        + `${ca.withdrawn} withdrawn, ${ca.unchanged} unchanged`
        + (ca.unparsed ? `, ${ca.unparsed} unparsed` : "")
        + (ca.failed ? `, ${ca.failed} failed` : "")
        + (ca.not_applicable ? `, ${ca.not_applicable} not applicable` : "")
        + (ca.skipped ? `, ${ca.skipped} skipped (no current security)` : "") + "."
      : "";
    const q = "Completed" in outcome ? outcome.Completed.quality_findings : 0;
    qualityLine = q ? `⚠ ${q} quality finding(s) — click the run below to see them.` : "";
  }

  // P6: lifecycle findings live outside any run (run_id NULL), so they get
  // their own list and their own on-demand check.
  let standaloneIssues = $state<IssueRow[]>([]);
  let lifecycleLine = $state("");

  async function loadStandaloneIssues() {
    try {
      standaloneIssues = await api.listStandaloneIssues();
    } catch (e) { error = String(e); }
  }

  async function lifecycleCheckNow() {
    if (inFlight) return;
    inFlight = true;
    lifecycleLine = "Asking Bloomberg…";
    try {
      const s: LifecycleSummary = await api.runLifecycleCheck();
      lifecycleLine = s.checked === 0
        ? "Nothing to check: every book instrument observed recently or was "
          + "checked within 30 days. 0 hits."
        : `${s.checked} checked, ${s.dead} not active, `
          + `${s.links_proposed} link(s) proposed, `
          + `${s.links_confirmed} auto-confirmed, ${s.issues} new issue(s).`;
      await loadStandaloneIssues();
    } catch (e) {
      lifecycleLine = "";
      error = String(e);
    } finally { inFlight = false; }
  }

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

  $effect(() => { loadViews(); loadStandaloneIssues(); });
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
        noteCorpActions(outcome);
        await Promise.all([loadViewData(), refreshRuns()]);
      }
    } catch (e) { error = String(e); }
    finally { inFlight = false; }
  }

  async function backfillGap(g: GapRow) {
    if (selectedViewId === null) return;
    inFlight = true;
    try {
      const ids = [g.instrument_id];
      const outcome = await api.runBackfillNow(selectedViewId, g.start, g.end, false, ids);
      if ("NeedsConfirmation" in outcome) {
        pending = { kind: "backfill", start: g.start, end: g.end, instrument_ids: ids,
                    ...outcome.NeedsConfirmation };
      } else {
        pending = null;
        noteCorpActions(outcome);
        await Promise.all([loadViewData(), refreshRuns()]);
      }
    } catch (e) { error = String(e); }
    finally { inFlight = false; }
  }

  async function confirmPending() {
    if (selectedViewId === null || pending === null) return;
    inFlight = true;
    try {
      const outcome = pending.kind === "eod"
        ? await api.runEodNow(selectedViewId, true)
        : await api.runBackfillNow(selectedViewId, pending.start, pending.end, true,
                                   pending.instrument_ids);
      noteCorpActions(outcome);
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

  {#if caLine}<p class="thin">{caLine}</p>{/if}
  {#if qualityLine}<p class="amber">{qualityLine}</p>{/if}

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
  <p class="thin">Per instrument, not per view: one member reporting on a date
     says nothing about the others. The Backfill button fetches only the
     instrument shown, for the range shown.</p>
  {#if !gaps.length}<p class="thin">No gaps.</p>{/if}
  <table>
    <thead><tr><th>Instrument</th><th>Start</th><th>End</th><th></th></tr></thead>
    <tbody>
      {#each gaps as g}
        <tr>
          <td>{g.label}</td><td>{g.start}</td><td>{g.end}</td>
          <td><button onclick={() => backfillGap(g)} disabled={inFlight}>Backfill</button></td>
        </tr>
      {/each}
    </tbody>
  </table>

  <h2>Lifecycle</h2>
  <p class="thin">Runs automatically after every completed run: book
     instruments with no data in 7 days get one MARKET_STATUS question
     (1 hit each, 30-day cooldown). A dead equity with Bloomberg-asserted
     merger terms links to its acquirer automatically; a dead fund is
     reported below — its successor must be picked by hand (CACX&nbsp;&lt;GO&gt;),
     then linked via Review.</p>
  <button onclick={lifecycleCheckNow} disabled={inFlight}>Check now</button>
  {#if lifecycleLine}<p class="notice">{lifecycleLine}</p>{/if}
  {#if standaloneIssues.length}
    <table class="static">
      <thead><tr><th>Instrument</th><th>Code</th><th>Detail</th></tr></thead>
      <tbody>
        {#each standaloneIssues as i}
          <tr>
            <td>{i.label ?? (i.instrument_id != null ? `instrument ${i.instrument_id}` : "—")}</td>
            <td>{i.code}</td>
            <td class="wrap">{i.detail}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {:else}
    <p class="thin">No standalone findings.</p>
  {/if}

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
  .thin { color: #666; font-size: 0.9em; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; width: 100%; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  tr.selected { background: #eef; }
  tbody tr { cursor: pointer; }
  .amber { color: #b8860b; font-weight: bold; }
  .red { color: #c00; font-weight: bold; }
  .confirm { border: 1px solid #c00; padding: 0.5rem; margin-top: 0.5rem; }
  .notice { color: #060; }
  .wrap { white-space: normal; max-width: 46rem; }
  table.static tbody tr { cursor: default; }
</style>
