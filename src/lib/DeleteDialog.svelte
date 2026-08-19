<script lang="ts">
  import { api, type DeleteMode, type DeletionImpact, type EntityKind } from "./api";

  let { kind, id, onclose }: {
    kind: EntityKind; id: number; onclose: (changed: boolean) => void;
  } = $props();

  let impact = $state<DeletionImpact | null>(null);
  let error = $state("");
  let busy = $state(false);

  $effect(() => {
    api.describeDeletion(kind, id)
      .then((i) => (impact = i))
      .catch((e) => (error = String(e)));
  });

  const NOUN: Record<EntityKind, string> = {
    asset_class: "asset class", asset: "asset", field: "field",
    view: "view", schedule: "schedule",
  };

  async function run(mode: DeleteMode) {
    busy = true; error = "";
    try {
      if (kind === "asset") await api.deleteAsset(id, mode);
      else if (kind === "field") await api.deleteField(id, mode);
      else if (kind === "view") await api.deleteView(id, mode);
      else if (kind === "asset_class") await api.deleteAssetClass(id);
      else await api.deleteSchedule(id);
      onclose(true);
    } catch (e) { error = String(e); busy = false; }
  }
</script>

<div class="backdrop">
  <div class="dialog">
    {#if !impact}
      {#if error}
        <p class="error">{error}</p>
        <div class="actions">
          <button onclick={() => onclose(false)}>Close</button>
        </div>
      {:else}
        <p>Checking what depends on this&hellip;</p>
        <div class="actions">
          <button onclick={() => onclose(false)}>Cancel</button>
        </div>
      {/if}
    {:else}
      {#if error}<p class="error">{error}</p>{/if}
      <h3>Remove {NOUN[kind]} &ldquo;{impact.label}&rdquo;?</h3>
      <ul class="counts">
        {#if impact.observations > 0}
          <li>
            {impact.observations} observation(s), {impact.first_obs} to {impact.last_obs}
            {#if impact.purge_keeps_history}
              &mdash; recorded against the underlying instrument; purge does not delete these
            {/if}
          </li>
        {/if}
        {#if impact.views > 0}<li>member of {impact.views} view(s)</li>{/if}
        {#if impact.issues > 0}
          <li>
            {impact.issues} recorded issue(s)
            {#if impact.purge_keeps_history}&mdash; also kept{/if}
          </li>
        {/if}
        {#if impact.runs > 0}<li>{impact.runs} run(s) reference it</li>{/if}
        {#if impact.children > 0}<li>{impact.children} dependent row(s)</li>{/if}
        {#if impact.observations === 0 && impact.views === 0 && impact.issues === 0
             && impact.runs === 0 && impact.children === 0}
          <li>nothing depends on it</li>
        {/if}
      </ul>
      {#if impact.blocked_reason}<p class="blocked">{impact.blocked_reason}</p>{/if}
      <div class="actions">
        {#if impact.can_retire}
          <button onclick={() => run("retire")} disabled={busy}>
            Retire &mdash; stop collecting, keep the data
          </button>
        {/if}
        {#if impact.can_purge}
          <button class="danger" onclick={() => run("purge")} disabled={busy}>
            {#if impact.purge_keeps_history}
              Purge &mdash; remove from the book (history is kept)
            {:else if impact.can_retire}
              Purge — delete it and its data
            {:else}
              Delete
            {/if}
          </button>
        {/if}
        <button onclick={() => onclose(false)} disabled={busy}>Cancel</button>
      </div>
      {#if impact.purge_keeps_history}
        <p class="note">
          Purge removes this from your book and from every view. The instrument, its
          identifiers and its recorded history are never deleted.
        </p>
      {:else if impact.can_purge && impact.can_retire}
        <p class="note">Purge cannot be undone. Runs and the budget ledger are never altered.</p>
      {/if}
    {/if}
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.35);
              display: flex; align-items: center; justify-content: center; }
  .dialog { background: #fff; border-radius: 4px; padding: 1.2rem;
            max-width: 34rem; box-shadow: 0 4px 20px rgba(0,0,0,0.3); }
  h3 { margin: 0 0 0.6rem; }
  .counts { margin: 0 0 0.8rem; padding-left: 1.2rem; color: #444; }
  .blocked { color: #a60; margin: 0 0 0.8rem; }
  .error { color: #c00; }
  .note { color: #666; font-size: 0.85rem; margin: 0.8rem 0 0; }
  .actions { display: flex; gap: 0.5rem; flex-wrap: wrap; }
  .danger { color: #c00; }
</style>
