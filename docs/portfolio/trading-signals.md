# Portfolio Trading Signals

The portfolio watcher generates two categories of email content beyond raw alerts:

| Category | Source | Trigger |
|---|---|---|
| **Swap Suggestions** | `analyzer::generate_swap_suggestions` | 7-day extreme + 30d SMA deviation |
| **Trading Insights** | `suggestions::generate_all_suggestions` | 6 independent signal engines |

Both appear in alert emails only — not in the console log or CLI output.

---

## Swap Suggestions (7-Day Extreme + 30-Day SMA)

The entry-point signal combining price history with the 30-day moving average.

**Sell candidate:** asset at a confirmed `New7dHigh` AND current price > 30d SMA  
**Buy candidate:** asset at a confirmed `New7dLow` AND current price < 30d SMA  
**Suggestion:** for every (sell, buy) pair found simultaneously → `SWAP sell FOR buy`

```
→ SWAP NVDAx FOR GOOGLx
  NVDAx: 7-day HIGH  price=€201.62  30d avg=€185.00  (+8.9% above avg)
  GOOGLx:  7-day LOW  price=€340.22  30d avg=€360.00  (-5.5% below avg)
  Positions: NVDAx €119  →  GOOGLx €126
```

**Data requirement:** full 7-day history window (10,080 snapshots) per asset + `BIRDEYE_API_KEY` for 30d SMA.

---

## Trading Insights

Six academic signal engines in `src/portfolio/suggestions.rs`. All are pure functions — no I/O — that receive the in-memory history deque and return `Vec<Suggestion>`.

---

### 1. Pairs Divergence

**Academic basis:** Gatev, Goetzmann & Rouwenhorst (2006) *"Pairs Trading: Performance of a Relative Value Arbitrage Rule"*, Journal of Finance.

**What it detects:** Two historically correlated assets have temporarily diverged beyond their normal spread range. The overperformer is expected to revert toward the underperformer.

**Signal computation:**
```
spread_t = ln(price_A / price_B)                 for each shared tick
spread_z = (current_spread − mean(spread)) / std(spread)

if |spread_z| > 2.0 → suggest SWAP overperformer FOR underperformer
```

**Monitored pairs:**

| Pair | Rationale |
|---|---|
| AAPLx / QQQx | AAPL is the largest QQQ component |
| GOOGLx / QQQx | GOOGL is a top QQQ constituent |
| NVDAx / QQQx | NVDA is a significant QQQ weight |
| TSLAx / QQQx | TSLA is in QQQ |
| NVDAx / SPYx | NVDA vs broad S&P 500 |
| AAPLx / SPYx | AAPL vs broad S&P 500 |
| GOOGLx / AAPLx | Two mega-cap tech peers |
| JitoSOL / SOL | Liquid staking — should track SOL tightly |

**Minimum data:** 120 aligned snapshots (2 hours) for each pair.

**Example output:**
```
[Pairs Divergence]
SWAP QQQx FOR NVDAx
  • NVDAx/QQQx log-price spread: z=+2.7σ — QQQx outperforming NVDAx by 3.2%
  • Spread deviates 2.7σ from 3240-sample rolling mean — reversion expected (Gatev et al., 2006)
```

---

### 2. RSI Extremes + Trend Confirmation

**Academic basis:** Wilder (1978) *"New Concepts in Technical Trading Systems"*. Empirically validated by Chong & Ng (2008) *"Technical analysis and the London Stock Exchange"*, Applied Economics Letters.

**What it detects:** Assets that are statistically overbought or oversold on a 14-period basis, with the 30-day SMA confirming that the extreme is not just a trending market.

**Signal computation:**
```
period = 840 minutes (14 × 60-minute bars)
RS     = avg_gain(period) / avg_loss(period)
RSI    = 100 − 100 / (1 + RS)

Sell if RSI > 70  AND  current_price > 30d SMA
Buy  if RSI < 30  AND  current_price < 30d SMA
```

The SMA filter is critical — without it, RSI can stay at 80+ for weeks during a genuine uptrend. The two-condition requirement significantly reduces false positives.

**Minimum data:** 841 snapshots (~14 hours) per asset. 30d SMA requires `BIRDEYE_API_KEY`.

**Example output:**
```
[RSI Extremes]
SWAP NVDAx FOR TSLAx
  • NVDAx RSI=76 (overbought >70) — price €201.62 vs 30d avg €185.00
  • TSLAx RSI=24 (oversold <30) — price €378.03 vs 30d avg €410.00
  • Both RSI extreme and SMA deviation confirm the signal (Wilder, 1978)
```

---

### 3. Sortino Ratio Rotation

**Academic basis:** Sortino & Price (1994) *"Performance Measurement in a Downside Risk Framework"*, Journal of Portfolio Management.

**What it detects:** Within the portfolio, the asset with the worst downside-adjusted returns relative to the best. Unlike the Sharpe ratio, Sortino only penalises negative returns — upside volatility is not counted against the asset.

**Signal computation:**
```
returns    = [ln(p_t / p_{t-1}) for each minute tick]
mean_r     = mean(returns)
DD         = sqrt(mean(min(r_t, 0)²))   — downside deviation
Sortino    = mean_r / DD

Suggest rotating from lowest-Sortino asset to highest-Sortino asset
when the difference is > 0.5
```

**Why Sortino over Sharpe here:** The xStock tokens and crypto assets have asymmetric, fat-tailed return distributions. Sharpe penalises a 5% upward spike the same as a 5% downside crash. Sortino correctly treats upside volatility as a positive attribute.

**Minimum data:** 120 returns (~2 hours) per asset.

**Example output:**
```
[Sortino Rotation]
SWAP TSLAx FOR AAPLx
  • TSLAx Sortino=−0.42 — worst downside-adjusted return (€115 position)
  • AAPLx Sortino=2.31 — best downside-adjusted return (€115 position)
  • Difference: 2.73 — unlike Sharpe, Sortino only penalises losses (Sortino & Price, 1994)
```

---

### 4. Information Ratio vs SOL

**Academic basis:** Grinold (1989) *"The Fundamental Law of Active Management"*, Journal of Portfolio Management.

**What it detects:** xToken positions that consistently underperform native SOL on a risk-adjusted basis. SOL is the natural benchmark for a Solana wallet — holding any other asset instead of SOL has an opportunity cost.

**Signal computation:**
```
excess_return_t = return(token_t) − return(SOL_t)   for each shared tick
IR              = mean(excess_returns) / std(excess_returns)

Suggest rotating to JitoSOL when IR < −0.30
```

JitoSOL is used as the rotation target because it provides SOL exposure plus liquid staking yield — a higher-IR alternative to raw SOL for a long-only portfolio.

**Minimum data:** 120 aligned returns for both the token and SOL. USDY is excluded (stablecoin; IR vs SOL is not meaningful for a yield instrument).

**Example output:**
```
[Information Ratio vs SOL]
CONSIDER SWAPPING GOOGLx FOR JitoSOL
  • GOOGLx IR=−0.82 vs SOL benchmark (threshold: −0.30) over 4320 observations
  • GOOGLx consistently underperforms SOL on risk-adjusted basis
  • JitoSOL = SOL exposure + liquid staking yield as a higher-IR alternative (Grinold, 1989)
  • Current position: €126
```

---

### 5. Volatility Squeeze

**Academic basis:** Brenner & Galai (1989) volatility index methodology; Bollinger (2002) *"Bollinger on Bollinger Bands"*. Low-volatility compression periods reliably precede sharp directional moves in both equity and crypto markets.

**What it detects:** Assets where recent (1-hour) realised volatility has contracted to less than 50% of the 24-hour baseline. This "squeeze" pattern indicates the market is coiling before a breakout. The 30d SMA determines the directional bias.

**Signal computation:**
```
recent_std   = std(log_returns over last 60 minutes)
baseline_std = std(log_returns over last 1440 minutes / 24 hours)
ratio        = recent_std / baseline_std

if ratio < 0.50:
    direction = "bullish" if current_price > 30d SMA, else "bearish"
    emit WATCH {asset} — vol squeeze ({direction})
```

Annualised volatility figures (`sqrt(var × 525_600) × 100`) are shown for interpretability.

**Minimum data:** 1,440 snapshots (1 day) per asset.

**Example output:**
```
[Volatility Squeeze]
WATCH NVDAx — vol squeeze (bullish)
  • 1h vol: 28.1%/yr  vs  24h baseline: 65.4%/yr  (EWMA: 42.7%/yr)  squeeze ratio=0.43
  • Vol at 43% of 24h average — compression before breakout (Bollinger, 2002)
  • Price above 30d SMA → bullish breakout bias
```

---

### 6. Bollinger Band Reversion

**Academic basis:** Bollinger (2002) *"Bollinger on Bollinger Bands"*. A price trading outside the ±2σ envelope around its moving average is statistically stretched and tends to revert toward the band.

**What it detects:** Assets whose current price has pierced their 30-day Bollinger band — above the upper band (overextended, sell candidate) or below the lower band (oversold, buy candidate). Unlike the Volatility Squeeze (#5), which measures band *width* contracting, this engine measures band *position*.

**Signal computation:**
```
upper = 30d SMA + K·σ        K = 2.0
lower = 30d SMA − K·σ
%B    = (price − lower) / (upper − lower)    0 = lower band, 1 = upper band

price ≥ upper (%B ≥ 1) → SELL candidate
price ≤ lower (%B ≤ 0) → BUY candidate
```

When a sell and a buy candidate co-exist, they are paired into `SWAP sell FOR buy` (same pairing as the RSI engine). A lone candidate with no partner emits a standalone `WATCH`.

**Minimum data:** 14 daily points for a meaningful σ (`BOLLINGER_MIN_DAYS`). The bands reuse the same 30-day daily series as the 30d SMA (local history, or Birdeye `1D` candles). Flat series (σ≈0, e.g. USDY) are skipped.

**Example output:**
```
[Bollinger Reversion]
SWAP NVDAx FOR GOOGLx
  • NVDAx €201.62 ≥ upper band €198.40 (30d, 2σ) — %B=1.08, stretched above envelope
  • GOOGLx €340.22 ≤ lower band €344.10 (30d, 2σ) — %B=−0.04, stretched below envelope
  • Price outside ±2σ Bollinger band — mean reversion expected (Bollinger, 2002)
```

---

## Configuration

| Parameter | Constant | Default | Notes |
|---|---|---|---|
| Pairs z-score threshold | `PAIRS_Z_THRESHOLD` | 2.0 σ | Increase to reduce sensitivity |
| Pairs minimum history | `PAIRS_MIN_OBS` | 120 snapshots | ~2 hours |
| RSI period | `RSI_PERIOD` | 840 | 14 × 60-minute bars |
| RSI oversold | `RSI_OVERSOLD` | 30 | Classic Wilder threshold |
| RSI overbought | `RSI_OVERBOUGHT` | 70 | Classic Wilder threshold |
| Sortino min history | `SORTINO_MIN_OBS` | 120 | ~2 hours |
| Sortino min difference | `SORTINO_MIN_DIFF` | 0.5 | Filters noise |
| IR threshold | `IR_THRESHOLD` | −0.30 | Flag underperformers |
| IR min history | `IR_MIN_OBS` | 120 | ~2 hours |
| Vol squeeze history | `SQUEEZE_MIN_HISTORY` | 1440 | 1 day baseline |
| Vol squeeze window | `SQUEEZE_RECENT_WINDOW` | 60 | 1 hour recent |
| Vol squeeze ratio | `SQUEEZE_RATIO_THRESHOLD` | 0.50 | <50% = squeeze |
| Bollinger σ multiplier | `BOLLINGER_K` | 2.0 | Band = 30d SMA ± k·σ; matches the CLI chart |
| Bollinger min daily points | `BOLLINGER_MIN_DAYS` | 14 | Below this, σ is too noisy to act on |

All constants are defined at the top of `src/portfolio/suggestions.rs`.

---

## Data Dependencies

| Signal | Min history | 30d SMA needed | SOL prices needed |
|---|---|---|---|
| Pairs Divergence | 120 snapshots | No | Only for JitoSOL/SOL pair |
| RSI Extremes | 841 snapshots | Optional (confirms signal) | No |
| Sortino Rotation | 120 snapshots | No | No |
| IR vs SOL | 120 snapshots | No | Yes |
| Volatility Squeeze | 1440 snapshots | Optional (direction) | No |
| Bollinger Reversion | 14 daily points | Yes (bands) | No |

The 30-day SMA is sourced from Birdeye daily candles at startup (requires `BIRDEYE_API_KEY`). Without it, RSI and Squeeze signals fire on price extremes alone (no SMA confirmation), and Swap Suggestions are disabled.

---

## References

- Bollinger, J. (2002). *Bollinger on Bollinger Bands*. McGraw-Hill.
- Brenner, M., & Galai, D. (1989). New financial instruments for hedging changes in volatility. *Financial Analysts Journal*, 45(4), 61–65.
- Chong, T. T. L., & Ng, W. K. (2008). Technical analysis and the London Stock Exchange. *Applied Economics Letters*, 15(13), 1111–1114.
- Gatev, E., Goetzmann, W. N., & Rouwenhorst, K. G. (2006). Pairs trading: Performance of a relative-value arbitrage rule. *Review of Financial Studies*, 19(3), 797–827.
- Grinold, R. C. (1989). The fundamental law of active management. *Journal of Portfolio Management*, 15(3), 30–37.
- Jegadeesh, N., & Titman, S. (1993). Returns to buying winners and selling losers: Implications for stock market efficiency. *Journal of Finance*, 48(1), 65–91.
- Sortino, F. A., & Price, L. N. (1994). Performance measurement in a downside risk framework. *Journal of Investing*, 3(3), 59–64.
- Wilder, J. W. (1978). *New Concepts in Technical Trading Systems*. Trend Research.
