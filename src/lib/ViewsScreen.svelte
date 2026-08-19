<script lang="ts">
  import { api, type Asset, type AssetClass, type EntityKind, type EstimateOut, type FieldDef, type View } from "./api";
  import DeleteDialog from "./DeleteDialog.svelte";

  let views = $state<View[]>([]);
  let assets = $state<Asset[]>([]);
  let fields = $state<FieldDef[]>([]);
  let classes = $state<AssetClass[]>([]);
  let estimates = $state<Record<number, EstimateOut>>({});
  let error = $state("");
  let pending = $state<{ kind: EntityKind; id: number } | null>(null);

  // A deletion can retire a row (it survives, just inactive) or purge it
  // (it's gone). Only purge invalidates a selection that points at it, so
  // reload first and then check whether the deleted id still exists before
  // clearing anything -- a plain retire should never silently uncheck a
  // selection the user didn't touch. Purging the selected view invalidates
  // everything the assign panel was showing, so that case clears all three.
  async function afterDelete(changed: boolean) {
    const deleted = pending;
    pending = null;
    if (!changed || !deleted) return;
    await reload();
    if (deleted.kind === "view" && !views.some((v) => v.id === deleted.id)) {
      selectedViewId = null;
      selectedAssetIds = [];
      selectedFieldIds = [];
    } else if (deleted.kind === "field" && !fields.some((f) => f.id === deleted.id)) {
      selectedFieldIds = selectedFieldIds.filter((id) => id !== deleted.id);
    }
  }

  let newViewName = $state("");
  let newViewDescription = $state("");

  let selectedViewId = $state<number | null>(null);
  let selectedAssetIds = $state<number[]>([]);
  let selectedFieldIds = $state<number[]>([]);

  let newField = $state({ asset_class_id: 0, mnemonic: "", label: "", value_kind: "numeric" });

  async function reload() {
    try {
      views = await api.listViews();
      assets = await api.listAssets();
      fields = await api.listFields();
      classes = await api.listAssetClasses();
      if (classes.length && !newField.asset_class_id) newField.asset_class_id = classes[0].id;
      const pairs = await Promise.all(views.map(async (v) => [v.id, await api.estimateView(v.id)] as const));
      estimates = Object.fromEntries(pairs);
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });

  async function addView() {
    try {
      await api.createView(newViewName, newViewDescription);
      newViewName = ""; newViewDescription = "";
      await reload();
    } catch (e) { error = String(e); }
  }

  async function selectView(id: number) {
    try {
      selectedViewId = id;
      const [va, vf] = await Promise.all([api.getViewAssets(id), api.getViewFields(id)]);
      selectedAssetIds = va.map((a) => a.id);
      selectedFieldIds = vf.map((f) => f.id);
    } catch (e) { error = String(e); }
  }

  function toggleAsset(id: number) {
    selectedAssetIds = selectedAssetIds.includes(id)
      ? selectedAssetIds.filter((x) => x !== id)
      : [...selectedAssetIds, id];
  }
  function toggleField(id: number) {
    selectedFieldIds = selectedFieldIds.includes(id)
      ? selectedFieldIds.filter((x) => x !== id)
      : [...selectedFieldIds, id];
  }

  async function saveAssignments() {
    if (selectedViewId === null) return;
    try {
      await api.setViewAssets(selectedViewId, selectedAssetIds);
      await api.setViewFields(selectedViewId, selectedFieldIds);
      await reload();
    } catch (e) { error = String(e); }
  }

  async function addField() {
    try {
      await api.createField(newField.asset_class_id, newField.mnemonic, newField.label, newField.value_kind);
      newField.mnemonic = ""; newField.label = "";
      await reload();
    } catch (e) { error = String(e); }
  }
</script>

{#if error}<p class="error">{error}</p>{/if}
<section>
  <h2>Views</h2>
  <form onsubmit={(e) => { e.preventDefault(); addView(); }}>
    <input bind:value={newViewName} placeholder="View name" required />
    <input bind:value={newViewDescription} placeholder="Description" />
    <button type="submit">Add view</button>
  </form>
  <table>
    <thead><tr><th>Name</th><th>Description</th><th>Estimate</th><th></th><th></th></tr></thead>
    <tbody>
      {#each views as v}
        <tr class:selected={selectedViewId === v.id}>
          <td>{v.name}</td>
          <td>{v.description}</td>
          <td>
            {#if estimates[v.id]}
              ~{estimates[v.id].estimated} hits (today: {estimates[v.id].today_total})
            {/if}
          </td>
          <td><button onclick={() => selectView(v.id)}>Select</button></td>
          <td><button class="x" title="Remove view"
                      onclick={() => (pending = { kind: "view", id: v.id })}>&times;</button></td>
        </tr>
      {/each}
    </tbody>
  </table>

  {#if selectedViewId !== null}
    <h2>Assign assets &amp; fields — {views.find((v) => v.id === selectedViewId)?.name}</h2>
    <div class="columns">
      <div>
        <h3>Assets</h3>
        <ul>
          {#each assets as a}
            <li>
              <label class:retired={!a.active}>
                <input type="checkbox" checked={selectedAssetIds.includes(a.id)}
                       onchange={() => toggleAsset(a.id)} />
                {a.label} ({a.bdp_security}){#if !a.active} &mdash; retired{/if}
              </label>
            </li>
          {/each}
        </ul>
      </div>
      <div>
        <h3>Fields</h3>
        <ul>
          {#each fields as f}
            <li>
              <label class:retired={!f.active}>
                <input type="checkbox" checked={selectedFieldIds.includes(f.id)}
                       onchange={() => toggleField(f.id)} />
                {f.mnemonic} — {f.label}{#if !f.active} &mdash; retired{/if}
              </label>
            </li>
          {/each}
        </ul>
      </div>
    </div>
    <button onclick={saveAssignments}>Save</button>
  {/if}

  <h2>Fields</h2>
  {#if !classes.length}
    <p class="hint">
      No asset class exists yet. Create one on the <strong>Assets</strong> tab first —
      a field is always defined for a class.
    </p>
  {/if}
  <!-- Deliberately not gated on selectedViewId: this is the only place a
       field can be removed, and a field must be removable even before any
       view exists (e.g. a fresh database with just a typo'd field). -->
  {#if fields.length}
    <table>
      <thead><tr><th>Mnemonic</th><th>Label</th><th>Class</th><th>Active</th><th></th></tr></thead>
      <tbody>
        {#each fields as f}
          <tr>
            <td>{f.mnemonic}</td>
            <td>{f.label}</td>
            <td>{classes.find((c) => c.id === f.asset_class_id)?.name}</td>
            <td><input type="checkbox" checked={f.active} disabled title="Retire/purge to change" /></td>
            <td><button class="x" title="Remove field"
                        onclick={() => (pending = { kind: "field", id: f.id })}>&times;</button></td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
  <form onsubmit={(e) => { e.preventDefault(); addField(); }}>
    <select bind:value={newField.asset_class_id} disabled={!classes.length}>
      {#each classes as c}<option value={c.id}>{c.name}</option>{/each}
    </select>
    <input bind:value={newField.mnemonic} placeholder="PX_LAST" required />
    <input bind:value={newField.label} placeholder="Label" required />
    <select bind:value={newField.value_kind}>
      <option value="numeric">numeric</option>
      <option value="text">text</option>
      <option value="date">date</option>
    </select>
    <button type="submit" disabled={!classes.length}>Add field</button>
  </form>
</section>

{#if pending}
  <DeleteDialog kind={pending.kind} id={pending.id} onclose={afterDelete} />
{/if}

<style>
  .error { color: #c00; }
  .hint { color: #a60; margin: 0.5rem 0 0; }
  .x { border: none; background: none; color: #c00; cursor: pointer;
       font-size: 1rem; line-height: 1; padding: 0 0.3rem; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  h3 { margin-bottom: 0.3rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  tr.selected { background: #eef; }
  form { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; margin-top: 0.5rem; }
  .columns { display: flex; gap: 2rem; }
  .columns ul { list-style: none; padding: 0; margin: 0; max-height: 16rem; overflow-y: auto; }
  .retired { color: #888; font-style: italic; }
</style>
