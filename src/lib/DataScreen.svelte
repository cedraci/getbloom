<script lang="ts">
  import { api, type AdjSeries, type AdjustModeStr, type BookEntry,
           type CorpActionFull, type FieldDef, type ObsRow,
           type StitchedSeries } from "./api";

  let book = $state<BookEntry[]>([]);
  let fields = $state<FieldDef[]>([]);
  let error = $state("");
  let notice = $state("");

  let instrumentId = $state<number | null>(null);
  let fieldId = $state<number | null>(null);
  let includeSuperseded = $state(false);
  let limit = $state(500);

  let observations = $state<ObsRow[]>([]);
  let corpActions = $state<CorpActionFull[]>([]);

  // P4: derived series. Raw shows the stored table; the other two derive
  // from the factor chain on read -- nothing is stored.
  let seriesMode = $state<AdjustModeStr>("raw");
  let adjSeries = $state<AdjSeries | null>(null);

  // P5: extend backward through confirmed merger/rename links.
  let canStitch = $state(false);
  let stitchOn = $state(false);
  let stitched = $state<StitchedSeries | null>(null);
  $effect(() => {
    const iid = instrumentId;
    if (iid === null) { canStitch = false; stitchOn = false; return; }
    api.hasConfirmedPredecessors(iid)
      .then((v) => { canStitch = v; if (!v) stitchOn = false; })
      .catch((e) => { error = String(e); });
  });
  const segLabel = (id: number) =>
    stitched?.segments.find((s) => s.instrument_id === id)?.label ?? `#${id}`;
  const segReport = (s: StitchedSeries) =>
    s.segments.map((g) => {
      const range = `${g.from ?? "…"} → ${g.to ?? "latest"}`;
      const ratio = g.ratio !== null && g.ratio !== 1 ? ` ×${g.ratio.toFixed(6)}` : "";
      // A roll splices by difference: the sign carries the meaning, so show it.
      const offset = g.offset !== null ? ` ${g.offset >= 0 ? "+" : ""}${g.offset}` : "";
      return `${g.label ?? "#" + g.instrument_id} (${range}${ratio}${offset}${g.note ? "; " + g.note : ""})`;
    }).join("  ·  ");

  // Fields that apply to the selected instrument's asset class.
  let classFields = $derived.by(() => {
    const entry = book.find((b) => b.instrument_id === instrumentId);
    if (!entry) return [] as FieldDef[];
    return fields.filter((f) => f.asset_class_id === entry.asset_class_id);
  });

  let obsPath = $state("");
  let caPath = $state("");
  let dataDir = $state("");

  async function init() {
    try {
      book = await api.listBook();
      fields = await api.listFields();
      dataDir = (await api.getSettings()).data_dir;
      if (book.length && instrumentId === null) instrumentId = book[0].instrument_id;
    } catch (e) { error = String(e); }
  }
  $effect(() => { init(); });

  // Keep the field selection valid for the chosen instrument, then load.
  $effect(() => {
    if (fieldId !== null && !classFields.some((f) => f.id === fieldId)) fieldId = null;
    if (fieldId === null && classFields.length) fieldId = classFields[0].id;
  });
  $effect(() => {
    // Reads all six inputs so any change reloads.
    instrumentId; fieldId; includeSuperseded; limit; seriesMode; stitchOn;
    load();
  });
  $effect(() => {
    if (instrumentId !== null && dataDir) {
      obsPath = stitchOn
        ? `${dataDir}\\stitched_${instrumentId}_${fieldId ?? "field"}_${seriesMode}.csv`
        : seriesMode === "raw"
          ? `${dataDir}\\obs_${instrumentId}_${fieldId ?? "field"}.csv`
          : `${dataDir}\\adj_${instrumentId}_${fieldId ?? "field"}_${seriesMode}.csv`;
      caPath = `${dataDir}\\corp_actions_${instrumentId}.csv`;
    }
  });

  async function load() {
    if (instrumentId === null) return;
    error = "";
    try {
      corpActions = await api.listCorpActionsFull(instrumentId, includeSuperseded);
      if (fieldId === null) {
        observations = []; adjSeries = null; stitched = null;
      } else if (stitchOn) {
        observations = []; adjSeries = null;
        stitched = await api.listStitched(instrumentId, fieldId, seriesMode, limit);
      } else if (seriesMode === "raw") {
        adjSeries = null; stitched = null;
        observations = await api.listObservations(instrumentId, fieldId,
                                                  includeSuperseded, limit);
      } else {
        observations = []; stitched = null;
        adjSeries = await api.listAdjusted(instrumentId, fieldId, seriesMode, limit);
      }
    } catch (e) { error = String(e); }
  }

  async function exportObs() {
    if (instrumentId === null || fieldId === null) return;
    notice = ""; error = "";
    try {
      const n = stitchOn
        ? await api.exportStitchedCsv(instrumentId, fieldId, seriesMode, obsPath)
        : seriesMode === "raw"
          ? await api.exportObservationsCsv(instrumentId, fieldId, obsPath)
          : await api.exportAdjustedCsv(instrumentId, fieldId, seriesMode, obsPath);
      notice = `${n} row(s) written to ${obsPath}`;
    } catch (e) { error = String(e); }
  }
  async function exportCa() {
    if (instrumentId === null) return;
    notice = ""; error = "";
    try {
      const n = await api.exportCorpActionsCsv(instrumentId, caPath);
      notice = `${n} corporate-action row(s) written to ${caPath}`;
    } catch (e) { error = String(e); }
  }

  const fmtValue = (o: ObsRow) => o.value_num ?? o.value_text ?? "—";
  const fmtBasis = (o: ObsRow) =>
    o.basis_note ? o.basis_note.split(" - ")[0] : (o.value_num !== null ? "?" : "—");
</script>

{#if error}<p class="error">{error}</p>{/if}
<section>
  <h2>Data</h2>
  <p class="thin">What the database actually stores, for checking against the
     Terminal. Read-only: this screen never calls Bloomberg.</p>

  {#if !book.length}
    <p class="hint">The book is empty — add instruments in the Book tab first.</p>
  {:else}
    <div class="controls">
      <label>Instrument
        <select bind:value={instrumentId}>
          {#each book as b}
            <option value={b.instrument_id}>{b.label} ({b.security ?? "—"})</option>
          {/each}
        </select>
      </label>
      <label>Field
        <select bind:value={fieldId} disabled={!classFields.length}>
          {#each classFields as f}<option value={f.id}>{f.mnemonic}</option>{/each}
        </select>
      </label>
      <label>Series
        <select bind:value={seriesMode}>
          <option value="raw">Raw (as stored)</option>
          <option value="splits">Split-adjusted</option>
          <option value="all">Split + dividend (net)</option>
        </select>
      </label>
      <label>Rows
        <input type="number" bind:value={limit} min="1" max="5000" />
      </label>
      <label class="check">
        <input type="checkbox" bind:checked={includeSuperseded}
               disabled={seriesMode !== "raw" || stitchOn} />
        show superseded (corrections history)
      </label>
      {#if canStitch}
        <label class="check">
          <input type="checkbox" bind:checked={stitchOn} />
          extend through confirmed mergers
        </label>
      {/if}
    </div>

    {#if notice}<p class="thin">{notice}</p>{/if}

    <h3>Observations</h3>
    {#if !classFields.length}
      <p class="hint">No field is defined for this instrument's asset class yet
         (Views tab → Fields).</p>
    {:else if stitchOn}
      {#if stitched === null || !stitched.rows.length}
        <p class="thin">Nothing stored for this instrument &amp; field.</p>
      {:else}
        <p class="thin">Spliced across confirmed links, in this instrument's
           units — derived on read, nothing is stored.<br />
           Segments: {segReport(stitched)}</p>
        {#if stitched.stopped}
          <p class="hint">⚠ extension stopped: {stitched.stopped}</p>
        {/if}
        <table>
          <thead><tr><th>Date</th><th>Value</th><th>Source</th></tr></thead>
          <tbody>
            {#each stitched.rows as r}
              <tr>
                <td>{r.obs_date}</td>
                <td class="num">{r.source_instrument_id === instrumentId
                                 ? r.value : r.value.toFixed(6)}</td>
                <td class="thin">{r.source_instrument_id === instrumentId
                                  ? "" : segLabel(r.source_instrument_id)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
        <div class="exportrow">
          <input bind:value={obsPath} />
          <button onclick={exportObs}>Export CSV</button>
        </div>
      {/if}
    {:else if seriesMode !== "raw"}
      {#if adjSeries === null || !adjSeries.rows.length}
        <p class="thin">Nothing stored for this instrument &amp; field.</p>
      {:else}
        <p class="thin">Derived on read from the stored factor chain
           ({adjSeries.factors_used} factor(s) applied) — nothing is stored.
           {#if adjSeries.unusable_factors}
             <span class="hint">⚠ {adjSeries.unusable_factors} stored factor
             row(s) could not be read and were NOT applied.</span>
           {/if}</p>
        <table>
          <thead><tr><th>Date</th><th>Raw</th>
                     <th>{seriesMode === "splits" ? "Split-adjusted" : "Net"}</th></tr></thead>
          <tbody>
            {#each adjSeries.rows as r}
              <tr>
                <td>{r.obs_date}</td>
                <td class="num">{r.raw}</td>
                <td class="num">{r.adjusted === r.raw ? r.raw : r.adjusted.toFixed(6)}</td>
              </tr>
            {/each}
          </tbody>
        </table>
        <div class="exportrow">
          <input bind:value={obsPath} />
          <button onclick={exportObs}>Export CSV</button>
        </div>
      {/if}
    {:else if !observations.length}
      <p class="thin">Nothing stored for this instrument &amp; field.</p>
    {:else}
      <table>
        <thead><tr><th>Date</th><th>Value</th><th>Ccy</th><th>Basis</th><th>Layer</th>
                   <th>Run</th><th>Recorded at</th><th></th></tr></thead>
        <tbody>
          {#each observations as o}
            <tr class:superseded={!o.current}>
              <td>{o.obs_date}</td>
              <td class="num">{fmtValue(o)}</td>
              <td class="thin">{o.currency ?? "—"}</td>
              <td title={o.basis_note ?? ""}>{fmtBasis(o)}</td>
              <td>{o.layer}</td>
              <td>{o.run_id}</td>
              <td class="thin">{o.system_from}</td>
              <td class="thin">{o.current ? "" : "superseded"}</td>
            </tr>
          {/each}
        </tbody>
      </table>
      <div class="exportrow">
        <input bind:value={obsPath} />
        <button onclick={exportObs}>Export CSV</button>
      </div>
    {/if}

    <h3>Corporate actions</h3>
    {#if !corpActions.length}
      <p class="thin">Nothing stored. Fetch with "Refresh corp actions" on a view
         (Views tab) or per instrument (Book tab → detail panel).</p>
    {:else}
      <table>
        <thead><tr><th>Field</th><th>Event date</th><th>Amount</th><th>Op/Flag</th>
                   <th>Type</th><th>Status</th><th>Pay date</th><th>Bloomberg row</th>
                   <th></th></tr></thead>
        <tbody>
          {#each corpActions as c}
            <tr class:superseded={!c.current}>
              <td class="thin">{c.source_field === "EQY_DVD_ADJUST_FACT" ? "factor" : "dividend"}</td>
              <td>{c.event_date ?? "—"}</td>
              <td class="num">{c.amount ?? "—"}</td>
              <td>{c.operator != null || c.flag != null ? `${c.operator ?? "—"}/${c.flag ?? "—"}` : "—"}</td>
              <td>{c.dvd_type ?? "—"}</td>
              <td>{c.amount_status ?? "—"}</td>
              <td>{c.pay_date ?? "—"}</td>
              <td>
                <details>
                  <summary>payload</summary>
                  <pre>{JSON.stringify(c.payload, null, 1)}</pre>
                </details>
              </td>
              <td class="thin">{c.current ? "" : "superseded"}</td>
            </tr>
          {/each}
        </tbody>
      </table>
      <div class="exportrow">
        <input bind:value={caPath} />
        <button onclick={exportCa}>Export CSV</button>
      </div>
    {/if}
  {/if}
</section>

<style>
  section { padding: 1rem; }
  .error { color: #c00; }
  .hint { color: #a60; }
  .thin { color: #666; font-size: 0.9em; }
  .num { font-family: ui-monospace, monospace; text-align: right; }
  .controls { display: flex; gap: 1rem; align-items: flex-end; flex-wrap: wrap; }
  .controls label { display: flex; flex-direction: column; font-size: 0.9em; }
  .controls label.check { flex-direction: row; gap: 0.4rem; align-items: center; }
  .controls input[type="number"] { width: 5rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; width: 100%; }
  th, td { border-bottom: 1px solid #eee; padding: 0.25rem 0.5rem; text-align: left; }
  tr.superseded { color: #999; }
  tr.superseded .num { text-decoration: line-through; }
  details pre { max-width: 28rem; max-height: 12rem; overflow: auto;
                background: #f7f7f7; padding: 0.4rem; font-size: 0.8em; }
  .exportrow { display: flex; gap: 0.5rem; margin: 0.4rem 0 1rem; }
  .exportrow input { flex: 1; max-width: 34rem; }
  h3 { margin: 1.2rem 0 0.2rem; }
</style>
