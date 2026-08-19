<script lang="ts">
  import { api, type AliasRow, type AttrRow } from "./api";

  let { instrumentId, onclose }: { instrumentId: number; onclose: () => void } = $props();

  let aliases = $state<AliasRow[]>([]);
  let attrs = $state<AttrRow[]>([]);
  let error = $state("");

  // The database's open-ended sentinel (see store::forever()) is a real,
  // finite date -- 9999-12-31, chosen because Postgres 'infinity' panics
  // chrono on decode and NaiveDate::MAX serializes to a string JavaScript
  // reads as Invalid Date. A lexicographic compare on the ISO string is
  // exactly right here and needs no date parsing.
  const OPEN_ENDED = "9999-12-31";
  const until = (d: string) => (d >= OPEN_ENDED ? "present" : d);

  async function load() {
    error = "";
    try {
      aliases = await api.instrumentAliases(instrumentId);
      attrs = await api.instrumentAttrs(instrumentId);
    } catch (e) { error = String(e); }
  }
  $effect(() => { load(); });
</script>

<div class="backdrop">
  <div class="panel">
    <button class="close" onclick={onclose}>Close</button>
    <h3>Instrument {instrumentId}</h3>
    {#if error}<p class="error">{error}</p>{/if}

    <h4>Identifiers</h4>
    <table>
      <thead><tr><th>Type</th><th>Value</th><th>From</th><th>Until</th>
                 <th>Source</th><th>Bloomberg event</th><th>Anchored to</th></tr></thead>
      <tbody>
        {#each aliases as a}
          <tr>
            <td>{a.id_type}</td>
            <td class="sec">{a.value}</td>
            <td>{a.valid_from}</td>
            <td>{until(a.valid_to)}</td>
            <td>{a.source}</td>
            <td>{a.bbg_action_id ?? "—"}</td>
            <td class="thin">{a.anchoring_identifier ?? "—"}</td>
          </tr>
        {/each}
      </tbody>
    </table>
    <p class="thin">Two rows for the same type are a change, not a duplicate:
       the earlier one ended when the later one began.</p>

    <h4>Attributes</h4>
    <table>
      <thead><tr><th>Attribute</th><th>Value</th><th>From</th><th>Until</th>
                 <th>Source</th></tr></thead>
      <tbody>
        {#each attrs as a}
          <tr><td>{a.attr}</td><td>{a.value}</td><td>{a.valid_from}</td>
              <td>{until(a.valid_to)}</td><td>{a.source}</td></tr>
        {/each}
      </tbody>
    </table>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.35);
              display: flex; align-items: center; justify-content: center; }
  .panel { background: #fff; border-radius: 4px; padding: 1.2rem;
           max-width: 60rem; max-height: 80vh; width: 90%; overflow: auto;
           box-shadow: 0 4px 20px rgba(0,0,0,0.3); }
  .close { float: right; }
  h3 { margin: 0 0 0.6rem; }
  h4 { margin: 1rem 0 0.4rem; }
  .error { color: #c00; }
  .sec { font-family: ui-monospace, monospace; }
  .thin { color: #666; font-size: 0.9em; }
  table { border-collapse: collapse; width: 100%; margin-bottom: 0.5rem; }
  th, td { border-bottom: 1px solid #eee; padding: 0.25rem 0.5rem; text-align: left; }
</style>
