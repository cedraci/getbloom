<script lang="ts">
  import { api, type BookEntry, type CorpActionFull, type FieldDef, type ObsRow } from "./api";

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
    // Reads all four inputs so any change reloads.
    instrumentId; fieldId; includeSuperseded; limit;
    load();
  });
  $effect(() => {
    if (instrumentId !== null && dataDir) {
      obsPath = `${dataDir}\\obs_${instrumentId}_${fieldId ?? "field"}.csv`;
      caPath = `${dataDir}\\corp_actions_${instrumentId}.csv`;
    }
  });

  async function load() {
    if (instrumentId === null) return;
    error = "";
    try {
      corpActions = await api.listCorpActionsFull(instrumentId, includeSuperseded);
      observations = fieldId === null ? []
        : await api.listObservations(instrumentId, fieldId, includeSuperseded, limit);
    } catch (e) { error = String(e); }
  }

  async function exportObs() {
    if (instrumentId === null || fieldId === null) return;
    notice = ""; error = "";
    try {
      const n = await api.exportObservationsCsv(instrumentId, fieldId, obsPath);
      notice = `${n} observation row(s) written to ${obsPath}`;
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
      <label>Rows
        <input type="number" bind:value={limit} min="1" max="5000" />
      </label>
      <label class="check">
        <input type="checkbox" bind:checked={includeSuperseded} />
        show superseded (corrections history)
      </label>
    </div>

    {#if notice}<p class="thin">{notice}</p>{/if}

    <h3>Observations</h3>
    {#if !classFields.length}
      <p class="hint">No field is defined for this instrument's asset class yet
         (Views tab → Fields).</p>
    {:else if !observations.length}
      <p class="thin">Nothing stored for this instrument &amp; field.</p>
    {:else}
      <table>
        <thead><tr><th>Date</th><th>Value</th><th>Basis</th><th>Layer</th>
                   <th>Run</th><th>Recorded at</th><th></th></tr></thead>
        <tbody>
          {#each observations as o}
            <tr class:superseded={!o.current}>
              <td>{o.obs_date}</td>
              <td class="num">{fmtValue(o)}</td>
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
