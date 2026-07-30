import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { confirm, save } from "@tauri-apps/plugin-dialog";

// Mirrors `PartGroupRow` in `src-tauri/src/lib.rs`.
interface PartGroupRow {
  index: number;
  display_name: string;
  references: string[];
  per_board_qty: number;
  is_passive: boolean;
}

interface PartsCredentials {
  mouser_api_key: string;
  digikey_client_id: string;
  digikey_client_secret: string;
}

// Mirrors `kicad_parse::bom_pricing::ChosenOffer`.
interface ChosenOffer {
  seller: string;
  manufacturer: string;
  mpn: string;
  sku: string;
  purchase_qty: number;
  unit_price: number;
  total_price: number;
  in_stock: boolean;
  stock_quantity: number;
  lifecycle_concern: boolean;
}

// Mirrors `GenerateBomEvent` (`src-tauri/src/lib.rs`), a JSON copy of
// `kicad_parse::generate_bom::BomEvent`.
type GenerateBomEvent =
  | { kind: "Log"; message: string }
  | { kind: "CurrentItem"; display_name: string }
  | {
      kind: "RowResult";
      index: number;
      needed_qty: number;
      outcome: { Ok: ChosenOffer } | { Err: string };
    }
  | { kind: "Done"; grand_total: number }
  | { kind: "InteractiveBomReady"; available: boolean };

interface RowOutcome {
  neededQty: number;
  outcome: { Ok: ChosenOffer } | { Err: string };
}

function outcomeSummary(row: RowOutcome): { glyph: string; text: string; className: string } {
  if ("Err" in row.outcome) {
    return { glyph: "✘", text: row.outcome.Err, className: "result-error" };
  }
  const chosen = row.outcome.Ok;
  const shortfall = chosen.stock_quantity < chosen.purchase_qty;
  const flagged = !chosen.in_stock || shortfall || chosen.lifecycle_concern;
  const note = !chosen.in_stock
    ? " (not in stock)"
    : shortfall
      ? " (not enough stock)"
      : "";
  return {
    glyph: flagged ? "⚠" : "✔",
    text: `Buy ${chosen.purchase_qty} — ${chosen.seller} @ $${chosen.unit_price.toFixed(2)} = $${chosen.total_price.toFixed(2)}${note}`,
    className: flagged ? "result-warning" : "result-ok",
  };
}

function defaultTimestamp(): string {
  return new Date()
    .toISOString()
    .replace(/[-:]/g, "")
    .replace(/\..+/, "")
    .replace("T", "_");
}

export function GenerateBom({ projectDir }: { projectDir: string }) {
  const [groups, setGroups] = useState<PartGroupRow[]>([]);
  const [boardQty, setBoardQty] = useState(1);
  const [passiveMarginPercent, setPassiveMarginPercent] = useState(20);
  const [forceRecheck, setForceRecheck] = useState(false);
  const [logLines, setLogLines] = useState<string[]>([]);
  const [status, setStatus] = useState<string | null>(null);
  const [inProgress, setInProgress] = useState(false);
  const [results, setResults] = useState<Map<number, RowOutcome>>(new Map());
  const [progressDone, setProgressDone] = useState(0);
  const [progressTotal, setProgressTotal] = useState(0);
  const [currentItem, setCurrentItem] = useState("");
  const [grandTotal, setGrandTotal] = useState<number | null>(null);
  const [ibomAvailable, setIbomAvailable] = useState(false);

  async function loadGroups() {
    if (!projectDir) return;
    const loaded = await invoke<PartGroupRow[]>("list_part_groups", { projectDir });
    setGroups(loaded);
    setResults(new Map());
    setProgressDone(0);
    setProgressTotal(0);
    setCurrentItem("");
    setGrandTotal(null);
    setIbomAvailable(false);
    setLogLines([
      `Found ${loaded.length} unique part(s) across ${loaded.reduce((n, g) => n + g.references.length, 0)} placed symbol(s).`,
    ]);
  }

  useEffect(() => {
    loadGroups();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectDir]);

  async function generate() {
    if (groups.length === 0) {
      setStatus("No parts found on the schematic to price.");
      return;
    }
    const settings = await invoke<PartsCredentials>("load_vendor_credentials");
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
    setGrandTotal(null);

    const projectName = projectDir.split(/[/\\]/).filter(Boolean).pop() ?? "project";

    const kicadOpen = await invoke<boolean>("check_kicad_open", { projectDir });
    if (kicadOpen) {
      const proceed = await confirm(
        `'${projectName}' appears to be open in KiCad.\n\n` +
          "Generate BOM can still look up pricing and produce the report, but " +
          "schematic changes (the cached lookup data used to skip repeat lookups) " +
          "will NOT be written back until you close it in KiCad.\n\nContinue anyway?",
        { title: "KiCad Has This Project Open", kind: "warning" },
      );
      if (!proceed) {
        setStatus(
          "Generate BOM cancelled — close the project in KiCad first, or " +
            "confirm the warning next time to proceed anyway.",
        );
        return;
      }
    }

    const timestamp = defaultTimestamp();
    const safeName = projectName.replace(/ /g, "_");
    const pdfPath = await save({
      defaultPath: `${projectDir}/${safeName}_bom_${timestamp}.pdf`,
      filters: [{ name: "PDF", extensions: ["pdf"] }],
    });
    const xlsxPath = await save({
      defaultPath: `${projectDir}/${safeName}_bom_${timestamp}.xlsx`,
      filters: [{ name: "Excel Workbook", extensions: ["xlsx"] }],
    });

    setProgressTotal(groups.length);
    setInProgress(true);

    const unlisten: UnlistenFn = await listen<GenerateBomEvent>(
      "generate-bom-event",
      (event) => {
        const payload = event.payload;
        switch (payload.kind) {
          case "Log":
            setLogLines((prev) => [...prev, payload.message]);
            break;
          case "CurrentItem":
            setCurrentItem(payload.display_name);
            break;
          case "RowResult":
            setProgressDone((prev) => prev + 1);
            setResults((prev) => {
              const next = new Map(prev);
              next.set(payload.index, {
                neededQty: payload.needed_qty,
                outcome: payload.outcome,
              });
              return next;
            });
            break;
          case "Done":
            setGrandTotal(payload.grand_total);
            break;
          case "InteractiveBomReady":
            setInProgress(false);
            setCurrentItem("");
            setIbomAvailable(payload.available);
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

    await invoke("generate_bom", {
      projectDir,
      boardQty,
      passiveMarginPercent,
      forceRecheck,
      kicadOpen,
      pdfPath,
      xlsxPath,
      credentials,
    });
  }

  return (
    <section className="card panel-card">
      <div className="panel-header">
        <h2>Generate BOM</h2>
        <button type="button" className="btn btn-sm" onClick={loadGroups} disabled={inProgress}>
          Reload
        </button>
      </div>

      <div className="toolbar">
        <label>
          Boards:
          <input
            type="number"
            min={1}
            value={boardQty}
            onChange={(e) => setBoardQty(Math.max(1, Number(e.currentTarget.value)))}
            disabled={inProgress}
            style={{ width: "5rem" }}
          />
        </label>
        <label>
          Passive extra margin:
          <input
            type="number"
            min={0}
            max={200}
            value={passiveMarginPercent}
            onChange={(e) => setPassiveMarginPercent(Number(e.currentTarget.value))}
            disabled={inProgress}
            style={{ width: "4rem" }}
          />
          %
        </label>
        <span className="field-hint">(resistors/capacitors/inductors only — min. +5 pcs)</span>
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
              <th>Part</th>
              <th>References</th>
              <th>Need</th>
              <th>Result</th>
            </tr>
          </thead>
          <tbody>
            {groups.map((group) => {
              const result = results.get(group.index);
              const previewNeeded = Math.max(
                group.per_board_qty * boardQty,
                group.per_board_qty,
              );
              return (
                <tr key={group.index}>
                  <td>{group.display_name}</td>
                  <td>{group.references.join(", ")}</td>
                  <td>{result ? result.neededQty : previewNeeded}</td>
                  <td>
                    {result &&
                      (() => {
                        const { glyph, text, className } = outcomeSummary(result);
                        return (
                          <span className={className}>
                            {glyph} {text}
                          </span>
                        );
                      })()}
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

      {grandTotal !== null && (
        <p className="status-line status-success">
          Estimated total: ${grandTotal.toFixed(2)}
        </p>
      )}

      <details className="log-panel">
        <summary>Detail Log</summary>
        <pre>{logLines.join("\n")}</pre>
      </details>

      <div className="form-actions">
        <button type="button" className="btn btn-primary" onClick={generate} disabled={inProgress}>
          {inProgress ? "Pricing…" : "Generate BOM"}
        </button>
        {ibomAvailable && (
          <button
            type="button"
            className="btn btn-secondary"
            onClick={() => invoke("open_interactive_bom")}
          >
            View Interactive BOM ↗
          </button>
        )}
      </div>
    </section>
  );
}
