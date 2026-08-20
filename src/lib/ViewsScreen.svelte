<script lang="ts">
  import { api, type AssetClass, type BookEntry, type EntityKind, type EstimateOut, type FieldDef, type View } from "./api";
  import DeleteDialog from "./DeleteDialog.svelte";

  let views = $state<View[]>([]);
  let book = $state<BookEntry[]>([]);
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
  let newClassName = $state("");

  let selectedViewId = $state<number | null>(null);
  let selectedAssetIds = $state<number[]>([]);
  let selectedFieldIds = $state<number[]>([]);

  let newField = $state({
    asset_class_id: 0, mnemonic: "", label: "", value_kind: "numeric",
    // Spec §4.9's configurable field-mapping layer. bbg_ftype records P0 §5's
    // BulkFormat marker (table-valued vs. scalar); all three are optional.
    bbg_ftype: "", bbg_datatype: "", entitlement_note: "",
  });

  async function reload() {
    try {
      views = await api.listViews();
      book = await api.listBook();
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
      const [va, vf]: [BookEntry[], FieldDef[]] =
        await Promise.all([api.getViewInstruments(id), api.getViewFields(id)]);
      selectedAssetIds = va.map((a) => a.instrument_id);
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

  // Whole-view corporate-actions refresh: batched (100 securities per
  // Bloomberg call), 2 hits per instrument, still an explicit user action.
  let caBusyViewId = $state<number | null>(null);
  let caNotice = $state("");

  async function refreshViewCorpActions(viewId: number) {
    error = ""; caNotice = "";
    caBusyViewId = viewId;
    try {
      const s = await api.refreshViewCorpActions(viewId);
      const name = views.find((v) => v.id === viewId)?.name ?? viewId;
      caNotice = `Corporate actions for "${name}": ${s.instruments} instrument(s) — `
        + `${s.inserted} new, ${s.amended} amended, ${s.withdrawn} withdrawn, `
        + `${s.unchanged} unchanged`
        + (s.unparsed ? `, ${s.unparsed} unparsed` : "")
        + (s.skipped ? `, ${s.skipped} skipped (no current security)` : "") + ".";
    } catch (e) { error = String(e); }
    finally { caBusyViewId = null; }
  }

  async function saveAssignments() {
    if (selectedViewId === null) return;
    try {
      await api.setViewInstruments(selectedViewId, selectedAssetIds);
      await api.setViewFields(selectedViewId, selectedFieldIds);
      await reload();
    } catch (e) { error = String(e); }
  }

  // Asset-class creation lived on the deleted AssetsScreen. It stays here
  // rather than moving to BookScreen because this is the only other place a
  // class is a hard prerequisite -- a field cannot be created without one.
  async function addClass() {
    try { await api.createAssetClass(newClassName, ""); newClassName = ""; await reload(); }
    catch (e) { error = String(e); }
  }

  async function addField() {
    try {
      await api.createField(newField.asset_class_id, newField.mnemonic, newField.label,
        newField.value_kind,
        newField.bbg_ftype || null, newField.bbg_datatype || null,
        newField.entitlement_note || null);
      newField.mnemonic = ""; newField.label = "";
      newField.bbg_ftype = ""; newField.bbg_datatype = ""; newField.entitlement_note = "";
      await reload();
    } catch (e) { error = String(e); }
  }
</script>

{#if error}<p class="error">{error}</p>{/if}
<section>
  <h2>Views</h2>
  {#if caNotice}<p class="note">{caNotice}</p>{/if}
  <form onsubmit={(e) => { e.preventDefault(); addView(); }}>
    <input bind:value={newViewName} placeholder="View name" required />
    <input bind:value={newViewDescription} placeholder="Description" />
    <button type="submit">Add view</button>
  </form>
  <table>
    <thead><tr><th>Name</th><th>Description</th><th>Estimate</th><th></th><th></th><th></th></tr></thead>
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
          <td><button onclick={() => refreshViewCorpActions(v.id)}
                      disabled={caBusyViewId !== null}
                      title="Fetch splits & dividend history for every member (2 Bloomberg hits per instrument)">
                {caBusyViewId === v.id ? "Asking Bloomberg…" : "Corp actions"}</button></td>
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
          {#each book as b}
            <li>
              <label class:retired={!b.active}>
                <input type="checkbox" checked={selectedAssetIds.includes(b.instrument_id)}
                       onchange={() => toggleAsset(b.instrument_id)} />
                {b.label} ({b.security ?? "—"}){#if !b.active} &mdash; retired{/if}
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

  <h2>Asset classes</h2>
  <p class="note">
    A class groups securities that share a field set (Equity, Corp, Index).
    Every book entry and every field belongs to one.
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

  <h2>Fields</h2>
  {#if !classes.length}
    <p class="hint">
      No asset class exists yet. Create one above first —
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
    <input bind:value={newField.bbg_ftype} placeholder="BulkFormat marker (optional)" />
    <input bind:value={newField.bbg_datatype} placeholder="Bloomberg datatype (optional)" />
    <input bind:value={newField.entitlement_note} placeholder="Entitlement note (optional)" />
    <button type="submit" disabled={!classes.length}>Add field</button>
  </form>
</section>

{#if pending}
  <DeleteDialog kind={pending.kind} id={pending.id} onclose={afterDelete} />
{/if}

<style>
  .error { color: #c00; }
  .hint { color: #a60; margin: 0.5rem 0 0; }
  .note { color: #555; margin: 0.2rem 0 0.6rem; max-width: 46rem; }
  .classes { list-style: none; padding: 0; margin: 0.5rem 0 0;
             display: flex; gap: 0.4rem; flex-wrap: wrap; }
  .classes li { border: 1px solid #ccc; border-radius: 3px; padding: 0.1rem 0.5rem; }
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
