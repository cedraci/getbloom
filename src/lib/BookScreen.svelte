<script lang="ts">
  import { api, type AssetClass, type BookEntry, type SearchHit } from "./api";
  import DeleteDialog from "./DeleteDialog.svelte";
  import ImportDiff from "./ImportDiff.svelte";
  import InstrumentDetail from "./InstrumentDetail.svelte";
  import type { EntityKind, ImportPlan } from "./api";

  let classes = $state<AssetClass[]>([]);
  let book = $state<BookEntry[]>([]);
  let error = $state(""), notice = $state("");

  let query = $state("");
  let hits = $state<SearchHit[]>([]);
  let yellowKey = $state("Equity");
  let classId = $state(0);
  let label = $state("");
  let searching = $state(false);
  let pending = $state<{ kind: EntityKind; id: number } | null>(null);
  let detailId = $state<number | null>(null);

  let sheetPath = $state("");
  let plan = $state<ImportPlan | null>(null);
  let previewBusy = $state(false);

  // Seeds sheetPath exactly once, at mount. Deliberately does not read sheetPath
  // itself, so the effect has no reactive dependency on it and never reasserts
  // the default over whatever the user typed. (Carried over from AssetsScreen.)
  async function seedSheetPath() {
    try { sheetPath = `${(await api.getSettings()).data_dir}\\book.xlsx`; }
    catch { sheetPath = "book.xlsx"; }
  }
  $effect(() => { seedSheetPath(); });

  async function exportSheet() {
    notice = ""; error = "";
    try { await api.exportAssetsXlsx(sheetPath); notice = `Written to ${sheetPath}`; }
    catch (e) { error = String(e); }
  }
  async function previewSheet() {
    previewBusy = true; notice = ""; error = "";
    try { plan = await api.previewAssetsImport(sheetPath); }
    catch (e) { error = String(e); }
    finally { previewBusy = false; }
  }
  function afterImport(applied: boolean, msg?: string) {
    plan = null;
    if (applied) { notice = msg ?? "Import applied."; reload(); }
  }

  function afterDelete(changed: boolean) {
    pending = null;
    if (changed) reload();
  }

  const YELLOW_KEYS = ["Equity", "Corp", "Govt", "Index", "Curncy", "Comdty",
                       "Mtge", "Muni", "Pfd"];

  const ORIGIN_LABEL: Record<string, string> = {
    book: "in your book",
    instrument: "known instrument",
    candidate: "seen before",
  };

  // Local search only. This runs on every keystroke and never calls Bloomberg;
  // the Bloomberg tier is the button below, and nothing else may trigger it.
  async function runLocalSearch() {
    const q = query.trim();
    if (!q) { hits = []; return; }
    try { hits = await api.searchLocal(q); } catch (e) { error = String(e); }
  }
  $effect(() => { query; runLocalSearch(); });

  async function searchBloomberg() {
    if (!query.trim()) return;
    searching = true; error = ""; notice = "";
    try {
      const r = await api.searchBloomberg(query, yellowKey);
      hits = r.hits;
      notice = `Bloomberg searched (${r.estimated_hits} hit charged); `
             + `${r.cached} result(s) cached — this search is free from now on.`;
    } catch (e) { error = String(e); }
    finally { searching = false; }
  }

  async function addHit(h: SearchHit) {
    error = ""; notice = "";
    try {
      const out = await api.addToBook({
        raw: h.security ?? h.display,
        yellow_key: yellowKey,
        asset_class_id: classId,
        label: label.trim() || h.display,
        hints: {},
      });
      if (out === "NotFound") {
        error = `Bloomberg does not recognise ${h.security ?? h.display}.`;
      } else if ("NeedsReview" in out) {
        notice = "Several securities match. It is waiting in the Review queue — "
               + "nothing has been added yet.";
      } else {
        notice = `Added ${out.Added.security ?? out.Added.label}.`;
        label = ""; query = "";
      }
      await reload();
    } catch (e) { error = String(e); }
  }

  async function reload() {
    try {
      classes = await api.listAssetClasses();
      book = await api.listBook();
      if (classes.length && !classId) classId = classes[0].id;
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });
</script>

{#if error}<p class="error">{error}</p>{/if}
{#if notice}<p class="notice">{notice}</p>{/if}

<section>
  <h2>Find an instrument</h2>
  <div class="row">
    <input bind:value={query} placeholder="AAPL, US0378331005, Apple…"
           aria-label="Search instruments" />
    <select bind:value={yellowKey}>
      {#each YELLOW_KEYS as k}<option>{k}</option>{/each}
    </select>
    <select bind:value={classId}>
      {#each classes as c}<option value={c.id}>{c.name}</option>{/each}
    </select>
    <input bind:value={label} placeholder="Your label (optional)" />
  </div>
  <p class="thin">Bonds: enter /isin/&lt;ISIN&gt; or a CT/GT generic — coupon-style
     tickers do not resolve, and instrument search does not cover government bonds.</p>

  {#if hits.length}
    <ul class="hits">
      {#each hits as h}
        <li>
          <span class="sec">{h.security ?? h.display}</span>
          <span class="desc">{h.description}</span>
          <span class="origin {h.origin}">{ORIGIN_LABEL[h.origin]}</span>
          {#if h.origin !== "book"}
            <button onclick={() => addHit(h)}>Add</button>
          {/if}
        </li>
      {/each}
    </ul>
  {:else if query.trim()}
    <p class="thin">Nothing local matches "{query}".</p>
  {/if}

  <!-- The only path to Bloomberg on this screen. Typing must never reach it. -->
  <button onclick={searchBloomberg} disabled={searching || !query.trim()}>
    {searching ? "Searching Bloomberg…" : "Search Bloomberg (1 hit)"}
  </button>
  <p class="thin">Typing costs nothing. This button asks Bloomberg once and keeps
     the answer forever.</p>
</section>

<section>
  <h2>Your book</h2>
  <table>
    <thead><tr><th>Label</th><th>Security</th><th>Class</th>
               <th>Active</th><th></th></tr></thead>
    <tbody>
      {#each book as b}
        <tr>
          <td>
            <button class="label-link" onclick={() => (detailId = b.instrument_id)}>
              {b.label}
            </button>
          </td>
          <td>{b.security ?? "—"}</td>
          <td>{classes.find((c) => c.id === b.asset_class_id)?.name ?? ""}</td>
          <td><input type="checkbox" checked={b.active}
                     onchange={(e) => api.setBookActive(b.instrument_id,
                        (e.currentTarget as HTMLInputElement).checked).then(reload)} /></td>
          <td><button class="x" title="Remove from book"
                      onclick={() => (pending = { kind: "asset", id: b.instrument_id })}>&times;</button></td>
        </tr>
      {/each}
    </tbody>
  </table>
</section>

<section>
  <h2>Excel</h2>
  <p class="thin">The export is also the migration tool: it is how a book survives
     a database rebuild.</p>
  <div class="row">
    <input bind:value={sheetPath} aria-label="Workbook path" />
    <button onclick={exportSheet}>Export</button>
    <button onclick={previewSheet} disabled={previewBusy}>
      {previewBusy ? "Reading…" : "Preview import"}</button>
  </div>
</section>

{#if plan}
  {#key plan}
    <ImportDiff {plan} path={sheetPath} onclose={afterImport} />
  {/key}
{/if}
{#if pending}
  <DeleteDialog kind={pending.kind} id={pending.id} onclose={afterDelete} />
{/if}
{#if detailId !== null}
  <InstrumentDetail instrumentId={detailId} onclose={() => (detailId = null)} />
{/if}

<style>
  .error { color: #c00; }
  .notice { color: #060; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  .row { display: flex; gap: 0.5rem; margin-bottom: 0.5rem; }
  .hits { list-style: none; padding: 0; }
  .hits li { display: flex; gap: 0.75rem; align-items: baseline;
             padding: 0.25rem 0; border-bottom: 1px solid #eee; }
  .sec { font-family: ui-monospace, monospace; }
  .desc { color: #555; flex: 1; }
  .origin { font-size: 0.8em; padding: 0 0.4em; border-radius: 3px; }
  .origin.book { background: #d8f0d8; }
  .origin.instrument { background: #e4e4f5; }
  .origin.candidate { background: #f0eada; }
  .thin { color: #666; font-size: 0.9em; }
  .x { border: none; background: none; color: #c00; cursor: pointer;
       font-size: 1rem; line-height: 1; padding: 0 0.3rem; }
  .label-link { border: none; background: none; padding: 0; font: inherit;
                color: #06c; text-decoration: underline; cursor: pointer; }
</style>
