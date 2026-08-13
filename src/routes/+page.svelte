<script lang="ts">
  import AssetsScreen from "$lib/AssetsScreen.svelte";
  import ViewsScreen from "$lib/ViewsScreen.svelte";
  import RunScreen from "$lib/RunScreen.svelte";
  import SettingsScreen from "$lib/SettingsScreen.svelte";
  let tab = $state<"assets" | "views" | "run" | "settings">("run");
</script>

<main>
  <nav>
    {#each [["run","Run"],["assets","Assets"],["views","Views"],["settings","Settings"]] as [id, label]}
      <button class:active={tab === id} onclick={() => (tab = id as typeof tab)}>{label}</button>
    {/each}
  </nav>
  {#if tab === "assets"}<AssetsScreen />{:else if tab === "views"}<ViewsScreen />
  {:else if tab === "run"}<RunScreen />{:else}<SettingsScreen />{/if}
</main>

<style>
  nav { display: flex; gap: 0.5rem; border-bottom: 1px solid #ccc; padding: 0.5rem; }
  nav button.active { font-weight: bold; text-decoration: underline; }
  main { font-family: system-ui, sans-serif; }
</style>
