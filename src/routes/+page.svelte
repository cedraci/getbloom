<script lang="ts">
  import BookScreen from "$lib/BookScreen.svelte";
  // TODO(Task 15): ReviewScreen.svelte does not exist yet -- it opens the
  // pending-review queue (list_pending_reviews / resolve_review / reject_review).
  // import ReviewScreen from "$lib/ReviewScreen.svelte";
  import ViewsScreen from "$lib/ViewsScreen.svelte";
  import RunScreen from "$lib/RunScreen.svelte";
  import SettingsScreen from "$lib/SettingsScreen.svelte";
  let tab = $state<"book" | "review" | "views" | "run" | "settings">("run");
</script>

<main>
  <nav>
    {#each [["run","Run"],["book","Book"],["review","Review"],
            ["views","Views"],["settings","Settings"]] as [id, label]}
      <button class:active={tab === id} onclick={() => (tab = id as typeof tab)}>{label}</button>
    {/each}
  </nav>
  {#if tab === "book"}<BookScreen />
  {:else if tab === "review"}<p class="placeholder">Review queue — coming in Task 15.</p>
  {:else if tab === "views"}<ViewsScreen />
  {:else if tab === "run"}<RunScreen />{:else}<SettingsScreen />{/if}
</main>

<style>
  nav { display: flex; gap: 0.5rem; border-bottom: 1px solid #ccc; padding: 0.5rem; }
  nav button.active { font-weight: bold; text-decoration: underline; }
  main { font-family: system-ui, sans-serif; }
  .placeholder { padding: 1rem; color: #666; }
</style>
