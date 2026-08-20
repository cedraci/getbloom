<script lang="ts">
  import BookScreen from "$lib/BookScreen.svelte";
  import ReviewScreen from "$lib/ReviewScreen.svelte";
  import ViewsScreen from "$lib/ViewsScreen.svelte";
  import RunScreen from "$lib/RunScreen.svelte";
  import DataScreen from "$lib/DataScreen.svelte";
  import SettingsScreen from "$lib/SettingsScreen.svelte";
  let tab = $state<"book" | "review" | "views" | "run" | "data" | "settings">("run");
</script>

<main>
  <nav>
    {#each [["run","Run"],["book","Book"],["review","Review"],
            ["views","Views"],["data","Data"],["settings","Settings"]] as [id, label]}
      <button class:active={tab === id} onclick={() => (tab = id as typeof tab)}>{label}</button>
    {/each}
  </nav>
  {#if tab === "book"}<BookScreen />
  {:else if tab === "review"}<ReviewScreen />
  {:else if tab === "views"}<ViewsScreen />
  {:else if tab === "data"}<DataScreen />
  {:else if tab === "run"}<RunScreen />{:else}<SettingsScreen />{/if}
</main>

<style>
  nav { display: flex; gap: 0.5rem; border-bottom: 1px solid #ccc; padding: 0.5rem; }
  nav button.active { font-weight: bold; text-decoration: underline; }
  main { font-family: system-ui, sans-serif; }
</style>
