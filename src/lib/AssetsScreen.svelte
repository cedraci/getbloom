<script lang="ts">
  import { api, type Asset, type AssetClass, type NewAsset } from "./api";
  import DeleteDialog from "./DeleteDialog.svelte";
  import ImportDiff from "./ImportDiff.svelte";
  import type { EntityKind, ImportPlan } from "./api";
  let classes = $state<AssetClass[]>([]);
  let assets = $state<Asset[]>([]);
  let error = $state("");
  let form = $state<NewAsset>({ asset_class_id: 0, label: "", id_kind: "ticker",
                                ticker: "", isin: null, yellow_key: "Equity" });
  let newClassName = $state("");
  let pending = $state<{ kind: EntityKind; id: number } | null>(null);

  let sheetPath = $state("");
  let plan = $state<ImportPlan | null>(null);
  let notice = $state("");

  function afterDelete(changed: boolean) {
    pending = null;
    if (changed) reload();
  }

  // Seeds sheetPath exactly once, at mount. Deliberately does not read
  // sheetPath itself, so the effect has no reactive dependency on it and
  // never re-runs when the user edits or clears the box -- an effect that
  // read `sheetPath` to decide whether to seed it would re-fire on every
  // keystroke and reassert the default over whatever the user typed.
  async function seedSheetPath() {
    try { sheetPath = `${(await api.getSettings()).data_dir}\\assets.xlsx`; }
    catch { sheetPath = "assets.xlsx"; }
  }
  $effect(() => { seedSheetPath(); });

  async function exportSheet() {
    notice = ""; error = "";
    try {
      await api.exportAssetsXlsx(sheetPath);
      notice = `Written to ${sheetPath}`;
    } catch (e) { error = String(e); }
  }
  let previewBusy = $state(false);
  async function previewSheet() {
    previewBusy = true; notice = ""; error = "";
    try { plan = await api.previewAssetsImport(sheetPath); }
    catch (e) { error = String(e); }
    finally { previewBusy = false; }
  }
  // `msg` is set by ImportDiff for both the workbook_refreshed success and
  // failure cases -- both are notices, never an error, per spec §8.2.
  function afterImport(applied: boolean, msg?: string) {
    plan = null;
    if (applied) { notice = msg ?? "Import applied."; reload(); }
  }

  async function reload() {
    try {
      classes = await api.listAssetClasses();
      assets = await api.listAssets();
      if (classes.length && !form.asset_class_id) form.asset_class_id = classes[0].id;
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });

  async function addClass() {
    try { await api.createAssetClass(newClassName, ""); newClassName = ""; await reload(); }
    catch (e) { error = String(e); }
  }
  async function addAsset() {
    try {
      await api.createAsset({ ...form,
        ticker: form.id_kind === "ticker" ? form.ticker : null,
        isin: form.id_kind === "isin" ? form.isin : null });
      await reload();
    } catch (e) { error = String(e); }
  }
</script>

{#if error}<p class="error">{error}</p>{/if}
<section>
  <h2>Asset classes</h2>
  <p class="note">
    A class groups securities that share a field set (Equity, Corp, Index).
    Every asset and every field belongs to one, so this is the first thing to create.
  </p>
  <input bind:value={newClassName} placeholder="e.g. Equity" />
  <button onclick={addClass} disabled={!newClassName}>Add class</button>
  <ul class="classes">
    {#each classes as c}
      <li>{c.name}
        <button class="x" title="Remove class"
                onclick={() => (pending = { kind: "asset_class", id: c.id })}>&times;</button>
      </li>
    {/each}
  </ul>

  <h2>Assets</h2>
  {#if !classes.length}
    <p class="hint">Add an asset class above before creating assets.</p>
  {/if}
  <form onsubmit={(e) => { e.preventDefault(); addAsset(); }}>
    <select bind:value={form.asset_class_id} disabled={!classes.length}>
      {#each classes as c}<option value={c.id}>{c.name}</option>{/each}
    </select>
    <input bind:value={form.label} placeholder="Label" required />
    <select bind:value={form.id_kind}>
      <option value="ticker">Ticker</option><option value="isin">ISIN</option>
    </select>
    {#if form.id_kind === "ticker"}
      <input bind:value={form.ticker} placeholder="AAPL US" required
             title="Ticker without the yellow key" />
    {:else}
      <input bind:value={form.isin} placeholder="FR0000120271" required />
    {/if}
    <input bind:value={form.yellow_key} placeholder="Equity / Corp / Index" required />
    <span class="preview">
      &rarr; {#if form.id_kind === "ticker" && form.ticker}{form.ticker.trim().replace(
          new RegExp("\s+" + form.yellow_key.trim() + "$", "i"), "")} {form.yellow_key.trim()}
        {:else if form.id_kind === "isin" && form.isin}/isin/{form.isin.trim()} {form.yellow_key.trim()}
        {:else}&hellip;{/if}
    </span>
    <button type="submit" disabled={!classes.length}>Add asset</button>
  </form>
  <table>
    <thead><tr><th>Label</th><th>Security</th><th>Class</th><th>Active</th><th></th></tr></thead>
    <tbody>
      {#each assets as a}
        <tr>
          <td>{a.label}</td><td>{a.bdp_security}</td>
          <td>{classes.find((c) => c.id === a.asset_class_id)?.name}</td>
          <td><input type="checkbox" checked={a.active}
               onchange={() => api.setAssetActive(a.id, !a.active).then(reload)} /></td>
          <td><button class="x" title="Remove asset"
                      onclick={() => (pending = { kind: "asset", id: a.id })}>&times;</button></td>
        </tr>
      {/each}
    </tbody>
  </table>

  <h2>Bulk edit in Excel</h2>
  <p class="note">
    Export writes every asset, its class and identifier, and one column per view.
    Edit it in Excel, then Preview to see exactly what would change before anything
    is applied. Leave <code>id</code> blank on a row to add an asset; delete a row
    to propose removing it. A sheet with no <code>id</code> column can only add and
    edit, never remove.
  </p>
  <div class="bulk">
    <input bind:value={sheetPath} size="48" />
    <button onclick={exportSheet}>Export</button>
    <button onclick={previewSheet} disabled={previewBusy}>Preview import</button>
  </div>
  {#if notice}<p class="notice">{notice}</p>{/if}
</section>

{#if pending}
  <DeleteDialog kind={pending.kind} id={pending.id} onclose={afterDelete} />
{/if}
{#if plan}
  {#key plan}
    <ImportDiff path={sheetPath} {plan} onclose={afterImport} />
  {/key}
{/if}

<style>
  .error { color: #c00; }
  .hint { color: #a60; margin: 0.5rem 0 0; }
  .note { color: #555; margin: 0.2rem 0 0.6rem; max-width: 46rem; }
  .preview { font-family: monospace; color: #060; }
  .bulk { display: flex; gap: 0.5rem; align-items: center; margin-top: 0.5rem; }
  .notice { color: #060; }
  .classes { list-style: none; padding: 0; margin: 0.5rem 0 0;
             display: flex; gap: 0.4rem; flex-wrap: wrap; }
  .classes li { border: 1px solid #ccc; border-radius: 3px; padding: 0.1rem 0.5rem; }
  .x { border: none; background: none; color: #c00; cursor: pointer;
       font-size: 1rem; line-height: 1; padding: 0 0.3rem; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  form { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; margin-top: 0.5rem; }
</style>
