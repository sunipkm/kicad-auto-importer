import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { confirm, save } from "@tauri-apps/plugin-dialog";
import { VendorDropdown, type ScoredCandidate } from "./VendorDropdown";

// Mirrors `CachedResult` in `src-tauri/src/lib.rs` — whatever a past
// lookup already wrote onto the instance, reconstructed without a
// network call.
interface CachedResult {
  available: boolean;
  stale: boolean;
  needs_attention: boolean;
  summary: string;
}

// Mirrors `PlacedSymbolRow` in `src-tauri/src/lib.rs`.
interface PlacedSymbolRow {
  index: number;
  reference: string;
  value: string;
  description: string;
  mpn: string;
  sch_path: string;
  uuid: string;
  cached: CachedResult;
}

// Mirrors `VendorCredentials`/`PartsCredentials` — see `SettingsPanel.tsx`.
interface PartsCredentials {
  mouser_api_key: string;
  digikey_client_id: string;
  digikey_client_secret: string;
}

// Mirrors `PopulateBomEvent` (`src-tauri/src/lib.rs`), which is itself a
// JSON-serializable copy of `kicad_auto_importer_core::populate_bom::LookupEvent`.
type PopulateBomEvent =
  | { kind: "Log"; message: string }
  | { kind: "CurrentItem"; reference: string }
  | {
      kind: "RowResult";
      index: number;
      ok: boolean;
      needs_attention: boolean;
      skipped: boolean;
      summary: string;
    }
  | { kind: "Done" };

interface RowResult {
  ok: boolean;
  needsAttention: boolean;
  skipped: boolean;
  /// Cached data whose `Last Checked` is past the recheck window — only
  /// set for a result seeded from `list_placed_symbols`'s `cached` on
  /// load, never from a `RowResult` event (a batch run either finds the
  /// part still fresh, in which case it isn't stale, or looks it up
  /// fresh itself).
  stale?: boolean;
  /// This tool has never looked the part up at all — distinct from
  /// `!ok` (a lookup that was attempted and failed).
  unavailable?: boolean;
  summary: string;
}

function resultGlyph(result: RowResult): string {
  if (result.unavailable) return "–"; // never checked
  if (!result.ok) return "✘"; // x
  if (result.stale || result.needsAttention) return "⚠"; // warning
  return result.skipped ? "⏸" : "✔"; // pause (fresh, not re-verified this run) / check
}

function resultClass(result: RowResult): string {
  if (result.unavailable) return "result-muted";
  if (!result.ok) return "result-error";
  if (result.stale || result.needsAttention) return "result-warning";
  return "result-ok";
}

export function PopulateBom({ projectDir }: { projectDir: string }) {
  const [rows, setRows] = useState<PlacedSymbolRow[]>([]);
  const [checked, setChecked] = useState<Set<number>>(new Set());
  const lastClicked = useRef<number | null>(null);
  const [forceRecheck, setForceRecheck] = useState(false);
  const [logLines, setLogLines] = useState<string[]>([]);
  const [status, setStatus] = useState<string | null>(null);
  const [inProgress, setInProgress] = useState(false);
  const [results, setResults] = useState<Map<number, RowResult>>(new Map());
  const [progressDone, setProgressDone] = useState(0);
  const [progressTotal, setProgressTotal] = useState(0);
  const [currentItem, setCurrentItem] = useState("");

  async function loadRows() {
    if (!projectDir) return;
    const loaded = await invoke<PlacedSymbolRow[]>("list_placed_symbols", {
      projectDir,
    });
    setRows(loaded);
    setChecked(new Set());
    // Auto-populate the Result column from whatever a past run already
    // wrote onto the schematic — no lookup needed to show it.
    const seeded = new Map<number, RowResult>();
    for (const row of loaded) {
      seeded.set(row.index, {
        ok: row.cached.available,
        needsAttention: row.cached.needs_attention,
        skipped: false,
        stale: row.cached.stale,
        unavailable: !row.cached.available,
        summary: row.cached.summary,
      });
    }
    setResults(seeded);
    setProgressDone(0);
    setProgressTotal(0);
    setCurrentItem("");
    setLogLines([`Found ${loaded.length} symbol(s) placed on the schematic.`]);
  }

  useEffect(() => {
    loadRows();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectDir]);

  // A manual vendor pick from `VendorDropdown` already wrote the choice
  // onto the schematic itself — this just reflects it in the Result
  // column immediately, without a full `loadRows()` (which would wipe
  // every row's result back to blank, since it re-lists placed symbols
  // from scratch and has no result history of its own to restore).
  function applyVendorResult(index: number, chosen: ScoredCandidate) {
    setResults((prev) => {
      const next = new Map(prev);
      const unsafe = !chosen.feasible || chosen.candidate.offer.lifecycle_concern;
      next.set(index, {
        ok: true,
        needsAttention: unsafe,
        skipped: false,
        summary: `${chosen.candidate.manufacturer} — ${chosen.candidate.offer.seller}${
          unsafe ? " (review before ordering)" : ""
        }`,
      });
      return next;
    });
  }

  function handleRowClick(i: number, shift: boolean) {
    setChecked((prev) => {
      const next = new Set(prev);
      if (shift && lastClicked.current !== null) {
        const [lo, hi] =
          lastClicked.current <= i
            ? [lastClicked.current, i]
            : [i, lastClicked.current];
        for (let idx = lo; idx <= hi; idx++) next.add(idx);
      } else {
        if (next.has(i)) {
          next.delete(i);
        } else {
          next.add(i);
        }
        lastClicked.current = i;
      }
      return next;
    });
  }

  async function populate() {
    if (checked.size === 0) {
      setStatus("Select at least one symbol first.");
      return;
    }
    const settings = await invoke<PartsCredentials>("load_global_settings");
    const digikeyConfigured =
      settings.digikey_client_id.trim() !== "" &&
      settings.digikey_client_secret.trim() !== "";
    if (settings.mouser_api_key.trim() === "" && !digikeyConfigured) {
      setStatus("Set a Mouser API key or a DigiKey Client ID/Secret first.");
      return;
    }
    setStatus(null);
    setResults(new Map());
    setProgressDone(0);
    setCurrentItem("");

    const projectName = projectDir.split(/[/\\]/).filter(Boolean).pop() ?? "project";

    const kicadOpen = await invoke<boolean>("check_kicad_open", { projectDir });
    if (kicadOpen) {
      const proceed = await confirm(
        `'${projectName}' appears to be open in KiCad.\n\n` +
          "Populate BOM can still look up stock/lifecycle info and generate the " +
          "report, but schematic changes will NOT be written back until you close " +
          "it in KiCad.\n\nContinue anyway?",
        { title: "KiCad Has This Project Open", kind: "warning" },
      );
      if (!proceed) {
        setStatus(
          "Populate BOM cancelled — close the project in KiCad first, or " +
            "confirm the warning next time to proceed anyway.",
        );
        return;
      }
    }

    const timestamp = new Date()
      .toISOString()
      .replace(/[-:]/g, "")
      .replace(/\..+/, "")
      .replace("T", "_");
    const defaultReportName = `${projectName.replace(/ /g, "_")}_stock_report_${timestamp}.pdf`;
    const reportPath =
      (await save({
        defaultPath: `${projectDir}/${defaultReportName}`,
        filters: [{ name: "PDF", extensions: ["pdf"] }],
      })) ?? `${projectDir}/${defaultReportName}`;

    setProgressTotal(checked.size);
    setInProgress(true);

    const unlisten: UnlistenFn = await listen<PopulateBomEvent>(
      "populate-bom-event",
      (event) => {
        const payload = event.payload;
        switch (payload.kind) {
          case "Log":
            setLogLines((prev) => [...prev, payload.message]);
            break;
          case "CurrentItem":
            setCurrentItem(payload.reference);
            break;
          case "RowResult":
            setProgressDone((prev) => prev + 1);
            setResults((prev) => {
              const next = new Map(prev);
              next.set(payload.index, {
                ok: payload.ok,
                needsAttention: payload.needs_attention,
                skipped: payload.skipped,
                summary: payload.summary,
              });
              return next;
            });
            break;
          case "Done":
            setInProgress(false);
            setCurrentItem("");
            unlisten();
            break;
        }
      },
    );

    const credentials: PartsCredentials = {
      mouser_api_key: settings.mouser_api_key,
      digikey_client_id: settings.digikey_client_id,
      digikey_client_secret: settings.digikey_client_secret,
    };

    await invoke("populate_bom", {
      projectDir,
      selectedIndices: Array.from(checked),
      forceRecheck,
      reportPath,
      kicadOpen,
      credentials,
    });
  }

  return (
    <section className="card">
      <div className="panel-header">
        <h2>Populate BOM</h2>
        <button type="button" className="btn btn-sm" onClick={loadRows} disabled={inProgress}>
          Reload
        </button>
      </div>

      <div className="toolbar">
        <button
          type="button"
          className="btn btn-sm"
          onClick={() => setChecked(new Set(rows.map((r) => r.index)))}
          disabled={inProgress}
        >
          Select All
        </button>
        <button
          type="button"
          className="btn btn-sm"
          onClick={() => setChecked(new Set())}
          disabled={inProgress}
        >
          Select None
        </button>
        <span className="toolbar-field">
          {checked.size} of {rows.length} selected
        </span>
        <div className="toolbar-spacer" />
        <label>
          <input
            type="checkbox"
            checked={forceRecheck}
            onChange={(e) => setForceRecheck(e.currentTarget.checked)}
            disabled={inProgress}
          />
          Force re-check
        </label>
      </div>

      <div className="table-scroll">
        <table className="data-table">
          <thead>
            <tr>
              <th></th>
              <th>Reference</th>
              <th>Value</th>
              <th>Description</th>
              <th>Result</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => {
              const result = results.get(row.index);
              return (
                <tr
                  key={row.index}
                  className={`selectable ${checked.has(row.index) ? "selected" : ""}`}
                  onClick={(e) => handleRowClick(row.index, e.shiftKey)}
                >
                  <td>
                    <input type="checkbox" readOnly checked={checked.has(row.index)} />
                  </td>
                  <td>{row.reference}</td>
                  <td>{row.value}</td>
                  <td>{row.description}</td>
                  <td className={result ? resultClass(result) : undefined}>
                    {result && (
                      <span>
                        {resultGlyph(result)} {result.summary}
                      </span>
                    )}
                  </td>
                  <td onClick={(e) => e.stopPropagation()}>
                    <VendorDropdown
                      mpn={row.mpn}
                      neededQty={1}
                      schPath={row.sch_path}
                      uuid={row.uuid}
                      currentSummary={result && !result.unavailable ? result.summary : null}
                      currentUnsafe={
                        !!result &&
                        !result.unavailable &&
                        (!result.ok || result.needsAttention || !!result.stale)
                      }
                      onApplied={(chosen) => applyVendorResult(row.index, chosen)}
                    />
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>

      {status && <p className="status-line status-error">{status}</p>}

      {progressTotal > 0 && (
        <div className="progress-row">
          {currentItem && <span>Looking up '{currentItem}'…</span>}
          <progress value={progressDone} max={progressTotal} />
          <span>
            {progressDone}/{progressTotal}
          </span>
        </div>
      )}

      <details className="log-panel">
        <summary>Detail Log</summary>
        <pre>{logLines.join("\n")}</pre>
      </details>

      <div className="form-actions">
        <button type="button" className="btn btn-primary" onClick={populate} disabled={inProgress}>
          {inProgress ? "Looking Up…" : "Populate BOM"}
        </button>
      </div>
    </section>
  );
}
