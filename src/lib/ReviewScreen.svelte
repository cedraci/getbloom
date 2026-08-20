<script lang="ts">
  import { api } from "./api";
  import type { PendingReview, LinkProposal, ScoredCandidate,
                LocalAmbiguityNote, ManualResolutionNote } from "./api";

  let reviews = $state<PendingReview[]>([]);
  let links = $state<LinkProposal[]>([]);
  let error = $state(""), notice = $state("");
  let showRaw = $state<number | null>(null);

  async function reload() {
    try {
      reviews = await api.listPendingReviews();
      links = await api.listLinkProposals();
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });

  async function choose(reviewId: number, security: string) {
    error = ""; notice = "";
    try {
      // resolve_review re-resolves `security` against Bloomberg for real --
      // it never binds the clicked string directly (spec §6.2) -- and
      // refuses a review that is no longer 'pending', so a double-submit
      // (two clicks, two tabs) surfaces here as an error rather than
      // silently minting a second instrument.
      await api.resolveReview(reviewId, security);
      notice = `Bound to ${security}.`;
      await reload();
    } catch (e) { error = String(e); }
  }

  // The free path. Spec §7: a Bloomberg call cannot resolve an ambiguity that
  // is entirely local, so none is made -- the review is closed by pointing at
  // one of the instruments that already exists.
  async function repoint(reviewId: number, instrumentId: number) {
    error = ""; notice = "";
    try {
      await api.resolveReviewLocal(reviewId, instrumentId);
      notice = `Re-pointed to instrument ${instrumentId}. No Bloomberg calls were made.`;
      await reload();
    } catch (e) { error = String(e); }
  }

  async function reject(reviewId: number) {
    error = ""; notice = "";
    try { await api.rejectReview(reviewId, "rejected by user"); await reload(); }
    catch (e) { error = String(e); }
  }

  async function confirm(linkId: number) {
    error = ""; notice = "";
    try { await api.confirmLink(linkId); notice = "Link confirmed."; await reload(); }
    catch (e) { error = String(e); }
  }

  // `candidates` is `serde_json::Value` on the Rust side, not a typed column
  // (see api.ts's ReviewCandidates comment) -- these three shapes are the
  // ones `resolution_decision.candidates` is actually written as. A pending
  // review's candidates are, as the engine stands today, always the scored
  // list (the other two never open a resolution_review row) -- but nothing
  // enforces that here, so every shape, including one none of these three
  // match, must render something rather than throw.
  // Checked BEFORE isScoredList. The engine used to write a local ambiguity as
  // a bare Vec<Scored>, indistinguishable on the wire from the Bloomberg
  // candidate list, so isScoredList matched it, this branch was unreachable
  // dead code, and the screen offered "This one" -- which spends a Bloomberg
  // call, and, where the security string was the `instrument #42` placeholder,
  // minted a permanent instrument wearing that literal text. The marker is
  // written explicitly now.
  function isLocalAmbiguity(c: unknown): c is LocalAmbiguityNote {
    return c !== null && typeof c === "object" && !Array.isArray(c)
      && (c as LocalAmbiguityNote).local_ambiguity === true
      && Array.isArray((c as LocalAmbiguityNote).candidates);
  }
  function isScoredList(c: unknown): c is ScoredCandidate[] {
    return Array.isArray(c) && c.every((x) =>
      x !== null && typeof x === "object" && "candidate" in x && "score" in x);
  }
  // The pre-marker shape, kept so a review opened by an older build still
  // renders something honest instead of falling through to raw JSON.
  function isLegacyLocalAmbiguityNote(c: unknown): c is LocalAmbiguityNote {
    return c !== null && typeof c === "object" && !Array.isArray(c)
      && "matched" in c && "bloomberg_calls" in c
      && !Array.isArray((c as LocalAmbiguityNote).candidates);
  }
  function isManualResolutionNote(c: unknown): c is ManualResolutionNote {
    return c !== null && typeof c === "object" && !Array.isArray(c)
      && "chosen_security" in c && "original_candidates" in c;
  }
</script>

{#if error}<p class="error">{error}</p>{/if}
{#if notice}<p class="notice">{notice}</p>{/if}

<section>
  <h2>Identifiers awaiting a decision</h2>
  {#if !reviews.length}
    <p class="thin">Nothing waiting. Every identifier resolved to exactly one security.</p>
  {/if}
  {#each reviews as r}
    <article>
      <h3>{r.raw_input} <span class="thin">→ {r.normalized}</span></h3>
      <p class="thin">Opened {new Date(r.opened_at).toLocaleString()}.
         Nothing is bound while this is open.</p>

      {#if isLocalAmbiguity(r.candidates)}
        <!-- Two or more instruments ALREADY in the database wear this
             identifier (e.g. BMW in Frankfurt and in the US). No Bloomberg
             call can tell them apart -- neither of them is unknown -- so none
             was made and none is needed to close this. The user says which
             one they meant, and that is a pure local re-point. -->
        <p class="thin">Matched locally on <strong>{r.candidates.matched}</strong>
           by {r.candidates.candidates.length} instruments already in the database.
           A Bloomberg call cannot resolve this — none was made, and choosing
           below costs nothing.</p>
        <table>
          <thead><tr><th>Instrument</th><th>Current security</th><th>Why</th><th></th></tr></thead>
          <tbody>
            {#each r.candidates.candidates as c}
              <tr>
                <td>{c.instrument_id ?? "—"}</td>
                <td class="sec">{c.candidate.security}</td>
                <td class="thin">{c.reasons.join("; ")}</td>
                <td>
                  {#if c.instrument_id != null}
                    <button onclick={() => repoint(r.review_id, c.instrument_id!)}>
                      This existing instrument (0 calls)</button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {:else if isScoredList(r.candidates)}
        <table>
          <thead><tr><th>Security</th><th>Description</th><th>Exchange</th>
                     <th>Score</th><th>Why</th><th></th></tr></thead>
          <tbody>
            {#each r.candidates as c}
              <tr class:out={c.disqualified}>
                <td class="sec">{c.candidate.security}</td>
                <td>{c.candidate.description}</td>
                <td>{c.candidate.exchange ?? "—"}</td>
                <td>{c.score}</td>
                <td class="thin">{c.reasons.join("; ")}</td>
                <td>
                  {#if !c.disqualified}
                    <button onclick={() => choose(r.review_id, c.candidate.security)}>
                      This one</button>
                  {/if}
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {:else if isLegacyLocalAmbiguityNote(r.candidates)}
        <!-- A review opened by a build that wrote the note without its
             candidate list. Nothing to pick from, so say so plainly. -->
        <p class="thin">Matched locally on <strong>{r.candidates.matched}</strong>
           by more than one instrument already in the database
           ({r.candidates.bloomberg_calls} Bloomberg call{r.candidates.bloomberg_calls === 1 ? "" : "s"} made).
           This review was recorded before the candidate list was stored, so
           there is nothing to pick from here — resolve the ambiguity in the
           book directly, or reject this review.</p>
      {:else if isManualResolutionNote(r.candidates)}
        <!-- The decision this note belongs to is itself the record of a past
             manual choice, not an open question -- shown so it is never
             mistaken for one. -->
        <p class="thin">Already resolved manually to
           <strong>{r.candidates.chosen_security}</strong>
           {#if r.candidates.bloomberg_fallback}(Bloomberg had nothing for it;
             bound from the chosen string alone){/if}. If this is still
           showing as pending, reject it rather than choosing again.</p>
      {:else}
        <!-- An unrecognised shape must never blank the screen: show the raw
             JSON and let the user reject rather than lose the review entirely. -->
        <p class="thin">Unrecognised candidates shape. Raw decision data:</p>
        <pre>{JSON.stringify(r.candidates, null, 2)}</pre>
      {/if}

      <button onclick={() => reject(r.review_id)}>None of these</button>
      <button onclick={() => (showRaw = showRaw === r.review_id ? null : r.review_id)}>
        {showRaw === r.review_id ? "Hide" : "Show"} what Bloomberg returned</button>
      {#if showRaw === r.review_id}
        {#if r.bbg_response === null}
          <p class="thin">No Bloomberg response recorded for this decision.</p>
        {:else}
          <pre>{JSON.stringify(r.bbg_response, null, 2)}</pre>
        {/if}
      {/if}
    </article>
  {/each}
</section>

<section>
  <h2>Proposed instrument links</h2>
  <p class="thin">Bloomberg exposes no successor field, so every link below was
     inferred. None of them is followed by any query until confirmed — the
     evidence below is what the user is being asked to agree is a corporate
     history, not merely a checkbox.</p>
  <p class="thin"><strong>Confirming a link extends the successor's stitched
     series backward through it</strong> (Data tab). Mergers Bloomberg itself
     asserts — an Action ID with parsed stock terms — are confirmed
     automatically and never appear here; what does appear is inferred, and
     needs your judgement. For a fund junction with no asserted ratio, the
     splice is derived from NAV continuity at the effective date — which is
     how fund merger ratios are officially set.</p>
  {#if !links.length}<p class="thin">No proposals.</p>{/if}
  {#each links as l}
    <article>
      <p>{l.predecessor_label ?? `instrument ${l.predecessor_id}`}
         → {l.successor_label ?? `instrument ${l.successor_id}`}
         ({l.link_type}, effective {l.effective_date})
         {#if l.exchange_ratio != null}
           — Bloomberg terms: {l.exchange_ratio} acquirer sh. per target sh.
         {/if}</p>
      <pre>{JSON.stringify(l.evidence, null, 2)}</pre>
      <button onclick={() => confirm(l.id)}>Confirm</button>
    </article>
  {/each}
</section>

<style>
  .error { color: #c00; }
  .notice { color: #060; }
  section { padding: 1rem; }
  h2 { margin-top: 1.5rem; }
  article { border: 1px solid #ddd; padding: 0.75rem; margin-bottom: 1rem; }
  table { border-collapse: collapse; margin-top: 0.5rem; }
  th, td { border: 1px solid #ccc; padding: 0.3rem 0.6rem; text-align: left; }
  .sec { font-family: ui-monospace, monospace; }
  .thin { color: #666; font-size: 0.9em; }
  tr.out { opacity: 0.5; }
  pre { background: #f6f6f6; padding: 0.5rem; overflow-x: auto; max-height: 20rem; }
</style>
