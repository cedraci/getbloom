# Asset-Class Playbook

Recommended capability flags and field configuration for each asset class. Adjust in **Settings → Asset classes**; per-field QC rules in **Views** screen; roll links in **Instrument detail**.

## Configuration Table

| Class | `corp_actions` | `ma_capable` | `adjustment_style` | `qc_stale_days_default` | Typical Fields |
|---|---|---|---|---|---|
| Equity | TRUE | TRUE | factors | NULL | PX_LAST, PX_VOLUME |
| Fund (weekly NAV) | FALSE | TRUE | factors | 8 | FUND_NET_ASSET_VAL |
| Index | FALSE | FALSE | none | NULL | PX_LAST |
| FX | FALSE | FALSE | none | NULL | PX_LAST |
| Future | FALSE | FALSE | none | NULL | PX_LAST, PX_VOLUME; rolls via manual links |
| Fixed Income | FALSE | FALSE | none | NULL | PX_LAST (clean), PX_DIRTY_MID, INT_ACC, YLD_YTM_MID |

## Why Each Class Deviates

**Equity** is the baseline: corp actions track splits/dividends, M&A is live, factor adjustments handle ex-dates.

**Fund (weekly NAV)** disables corp actions (never applicable to fund shares). Keeps `ma_capable = TRUE`: fund absorption follows the same investigation entry point as equity M&A. QC stale at 8 days enforces weekly deposit cadence.

**Index** disables corp actions (indices do not distribute) and M&A (no corporate events). No factor adjustments: indices are calculated snapshots, not adjusted series.

**FX** disables all corporate logic: FX pairs have no actions, no M&A, no adjustments—clean spot rates only.

**Future** disables corp actions and M&A (futures do not participate). No adjustments: contract terms set on issuance. Rolls are manual links only, not inherited from the underlying.

**Fixed Income** disables corp actions (bonds distribute via interest, not corp events) and M&A (bond series close via `INACTIVE_DATE` or default, not acquisition). No adjustments: YTM and dirty prices reflect accrual directly; the factor engine is dividend/split arithmetic, not applicable.

## Must-State Caveats

1. **Yield fields stay QC-permissive:** YLD_YTM_MID and similar keep `qc_nonpositive = FALSE`. Negative yields are real (rare, but real); do not flag them as stale data.

2. **GBp prices stored verbatim:** Per P7 decision, prices in pence (GBp) are stored and displayed as-is. No conversion to GBX; pence stay pence.

3. **Called bonds need no link:** A bond series with `ma_capable = FALSE` + Bloomberg's `INACTIVE_DATE` field auto-cap the series without a manual M&A link (see spec 9.3). Do not create a link for redemption or call events.

4. **Funds inherit the absorption path:** Funds stay `ma_capable = TRUE` because fund absorption (scheme merger, unitisation change) is detected by the same investigation logic as equity M&A. The entry point is shared; the payoff is fund holders identify with the new series automatically.
