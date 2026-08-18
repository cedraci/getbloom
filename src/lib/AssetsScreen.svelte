<script lang="ts">
  import { api, type Asset, type AssetClass, type NewAsset } from "./api";
  let classes = $state<AssetClass[]>([]);
  let assets = $state<Asset[]>([]);
  let error = $state("");
  let form = $state<NewAsset>({ asset_class_id: 0, label: "", id_kind: "ticker",
                                ticker: "", isin: null, yellow_key: "Equity" });
  let newClassName = $state("");

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
    {#each classes as c}<li>{c.name}</li>{/each}
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
    <thead><tr><th>Label</th><th>Security</th><th>Class</th><th>Active</th></tr></thead>
    <tbody>
      {#each assets as a}
        <tr>
          <td>{a.label}</td><td>{a.bdp_security}</td>
          <td>{classes.find((c) => c.id === a.asset_class_id)?.name}</td>
          <td><input type="checkbox" checked={a.active}
               onchange={() => api.setAssetActive(a.id, !a.active).then(reload)} /></td>
        </tr>
      {/each}
    </tbody>
  </table>
</section>

<style>
  .error { color: #c00; }
  .hint { color: #a60; margin: 0.5rem 0 0; }
  .note { color: #555; margin: 0.2rem 0 0.6rem; max-width: 46rem; }
  .preview { font-family: monospace; color: #060; }
  .classes { list-style: none; padding: 0; margin: 0.5rem 0 0;
             display: flex; gap: 0.4rem; flex-wrap: wrap; }
  .classes li { border: 1px solid #ccc; border-radius: 3px; padding: 0.1rem 0.5rem; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  form { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; margin-top: 0.5rem; }
</style>
