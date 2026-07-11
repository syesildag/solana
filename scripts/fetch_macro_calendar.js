#!/usr/bin/env node
/**
 * fetch_macro_calendar.js — refresh assets/macro_calendar.json (CPI/PPI/FOMC
 * release timestamps used by the momentum trader's macro-blackout entry gate,
 * MOMENTUM_MACRO_BLACKOUT_HOURS / _AFTER_HOURS).
 *
 * Sources (each falls back to the existing file's entries when unreachable, so a
 * partial refresh never LOSES protection):
 *   - FOMC: federalreserve.gov meeting calendar (keyless scrape; decision = day 2
 *     of each meeting at 14:00 ET).
 *   - CPI (FRED release 10) + PPI (FRED release 46): the FRED release-dates API
 *     with `include_release_dates_with_no_data=true`, which returns the FUTURE
 *     announced schedule. Needs a free key in FRED_API_KEY (https://fred.stlouisfed.org
 *     → My Account → API Keys). BLS.gov itself bot-blocks (403/406), so FRED is
 *     the sanctioned machine-readable mirror of the same schedule.
 *
 * Release times are constants (CPI/PPI 08:30 ET, FOMC statement 14:00 ET),
 * converted to UTC with the US DST rule (second Sunday of March → first Sunday
 * of November).
 *
 * Usage:
 *   node scripts/fetch_macro_calendar.js            # refresh in place
 *   node scripts/fetch_macro_calendar.js --output FILE
 */
"use strict";

const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");

function argVal(flag, dflt) {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : dflt;
}
const OUTPUT = argVal("--output", path.join(ROOT, "assets", "macro_calendar.json"));

// Minimal .env reader (repo convention: scripts stay dependency-free). Real env wins.
function envVal(key) {
  if (process.env[key]) return process.env[key];
  try {
    const m = fs
      .readFileSync(path.join(ROOT, ".env"), "utf8")
      .match(new RegExp(`^${key}=(.*)$`, "m"));
    return m ? m[1].trim() : undefined;
  } catch {
    return undefined;
  }
}

const MONTHS = {
  january: 1, february: 2, march: 3, april: 4, may: 5, june: 6,
  july: 7, august: 8, september: 9, october: 10, november: 11, december: 12,
};

// US DST: in effect from the second Sunday of March to the first Sunday of November.
function inUsDst(y, m, d) {
  const nthSunday = (year, month, n) => {
    const first = new Date(Date.UTC(year, month - 1, 1)).getUTCDay();
    return 1 + ((7 - first) % 7) + (n - 1) * 7;
  };
  const start = nthSunday(y, 3, 2), end = nthSunday(y, 11, 1);
  if (m > 3 && m < 11) return true;
  if (m === 3) return d >= start;
  if (m === 11) return d < end;
  return false;
}

// Release moment in epoch seconds: hourEt on (y,m,d), ET→UTC via the DST rule.
function releaseTs(y, m, d, hourEt, minEt) {
  const offset = inUsDst(y, m, d) ? 4 : 5; // EDT / EST
  return Math.floor(Date.UTC(y, m - 1, d, hourEt + offset, minEt) / 1000);
}

function toEvent(name, y, m, d, hourEt, minEt) {
  const ts = releaseTs(y, m, d, hourEt, minEt);
  return { name, utc: new Date(ts * 1000).toISOString().replace(".000", ""), ts };
}

async function fetchFomc() {
  const res = await fetch("https://www.federalreserve.gov/monetarypolicy/fomccalendars.htm", {
    headers: { "User-Agent": "Mozilla/5.0 (macro-calendar-updater)" },
  });
  if (!res.ok) throw new Error(`fed HTTP ${res.status}`);
  const html = await res.text();
  const events = [];
  // The page is organized in per-year panels ("2026 FOMC Meetings") holding
  // month + day-range cells. Walk it linearly, tracking the current year.
  let year = 0;
  const re = /(\d{4}) FOMC Meetings|fomc-meeting__month[^>]*>\s*(?:<[^>]+>)*([A-Za-z/]+)|fomc-meeting__date[^>]*>([^<]*)/g;
  let month = null, m;
  while ((m = re.exec(html))) {
    if (m[1]) { year = parseInt(m[1], 10); continue; }
    if (m[2]) { month = m[2]; continue; }
    if (m[3] && month && year) {
      const range = m[3].replace(/\*| /g, "").trim(); // "27-28", "30-1", "3-4 (unscheduled)"
      if (!range || /unscheduled|notation/i.test(m[3])) { month = null; continue; }
      const days = range.match(/\d{1,2}/g);
      if (!days) { month = null; continue; }
      const day2 = parseInt(days[days.length - 1], 10);
      // "April/May" style cross-month meetings: the decision day belongs to the
      // SECOND month whenever day2 wrapped below day1.
      const parts = month.toLowerCase().split("/");
      let mm = MONTHS[parts[0]];
      if (parts.length === 2 && parseInt(days[0], 10) > day2) mm = MONTHS[parts[1]];
      if (mm) events.push(toEvent(`FOMC ${month.slice(0, 3)} ${year}`, year, mm, day2, 14, 0));
      month = null;
    }
  }
  if (!events.length) throw new Error("fed page parsed to zero meetings (layout change?)");
  return events;
}

async function fetchFredRelease(releaseId, label, apiKey) {
  const url =
    `https://api.stlouisfed.org/fred/release/dates?release_id=${releaseId}` +
    `&include_release_dates_with_no_data=true&realtime_start=2020-01-01&realtime_end=9999-12-31` +
    `&sort_order=desc&limit=60&api_key=${apiKey}&file_type=json`;
  const res = await fetch(url);
  if (!res.ok) throw new Error(`FRED release ${releaseId} HTTP ${res.status}`);
  const body = await res.json();
  const dates = (body.release_dates || []).map((r) => r.date);
  if (!dates.length) throw new Error(`FRED release ${releaseId} returned no dates`);
  return dates.map((iso) => {
    const [y, mo, d] = iso.split("-").map(Number);
    return toEvent(`${label} ${iso}`, y, mo, d, 8, 30);
  });
}

(async () => {
  // Existing entries, grouped by kind, are each source's fallback.
  let existing = [];
  try {
    existing = JSON.parse(fs.readFileSync(OUTPUT, "utf8"));
  } catch {
    // First run against the legacy filename.
    try {
      existing = JSON.parse(fs.readFileSync(path.join(ROOT, "assets", "macro_calendar_2026.json"), "utf8"));
    } catch {}
  }
  const keep = (prefix) => existing.filter((e) => e.name.startsWith(prefix));

  let fomc, cpi, ppi;
  try {
    fomc = await fetchFomc();
    console.log(`FOMC: ${fomc.length} meetings from federalreserve.gov`);
  } catch (e) {
    fomc = keep("FOMC");
    console.warn(`FOMC fetch failed (${e.message}) — keeping ${fomc.length} existing entries`);
  }

  const key = envVal("FRED_API_KEY");
  if (key) {
    try {
      cpi = await fetchFredRelease(10, "CPI", key);
      console.log(`CPI: ${cpi.length} release dates from FRED`);
    } catch (e) {
      cpi = keep("CPI");
      console.warn(`CPI fetch failed (${e.message}) — keeping ${cpi.length} existing entries`);
    }
    try {
      ppi = await fetchFredRelease(46, "PPI", key);
      console.log(`PPI: ${ppi.length} release dates from FRED`);
    } catch (e) {
      ppi = keep("PPI");
      console.warn(`PPI fetch failed (${e.message}) — keeping ${ppi.length} existing entries`);
    }
  } else {
    cpi = keep("CPI");
    ppi = keep("PPI");
    console.warn(
      `FRED_API_KEY not set — keeping ${cpi.length} CPI + ${ppi.length} PPI existing entries.\n` +
      `  Get a free key at https://fred.stlouisfed.org (My Account → API Keys) and add FRED_API_KEY=... to .env`
    );
  }

  // Merge, de-dupe by (kind, ts), drop events older than ~14 months (keep enough
  // past for extended-history backtests), sort.
  const cutoff = Math.floor(Date.now() / 1000) - 425 * 86_400;
  const seen = new Set();
  const all = [...fomc, ...cpi, ...ppi]
    .filter((e) => e.ts >= cutoff)
    .filter((e) => {
      const k = `${e.name.slice(0, 3)}:${e.ts}`;
      if (seen.has(k)) return false;
      seen.add(k);
      return true;
    })
    .sort((a, b) => a.ts - b.ts);

  if (!all.length) {
    console.error("refusing to write an empty calendar");
    process.exit(1);
  }
  fs.writeFileSync(OUTPUT, JSON.stringify(all, null, 1) + "\n");
  const last = all[all.length - 1];
  console.log(`Wrote ${all.length} events → ${OUTPUT} (last: ${last.name} @ ${last.utc})`);
  const daysLeft = Math.floor((last.ts - Date.now() / 1000) / 86_400);
  if (daysLeft < 45) {
    console.warn(`WARNING: calendar ends in ${daysLeft}d — sources may not have published further schedules yet; re-run later.`);
  }
})().catch((e) => {
  console.error(`fetch_macro_calendar failed: ${e.message}`);
  process.exit(1);
});
