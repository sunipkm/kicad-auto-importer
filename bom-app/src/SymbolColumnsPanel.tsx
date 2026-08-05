import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

// Mirrors `symbol_columns::{SymbolColumn, SymbolColumnEntry, SymbolColumnsConfig}`.
type SymbolColumnKey = "Reference" | "Value" | "Description" | "Footprint" | "Mpn";

interface SymbolColumnEntry {
  column: SymbolColumnKey;
  visible: boolean;
}

interface SymbolColumnsConfig {
  entries: SymbolColumnEntry[];
}

const COLUMN_LABEL: Record<SymbolColumnKey, string> = {
  Reference: "Reference",
  Value: "Value",
  Description: "Description",
  Footprint: "Footprint",
  Mpn: "MPN",
};

const MANDATORY: Set<SymbolColumnKey> = new Set(["Reference"]);

export function SymbolColumnsPanel() {
  const [entries, setEntries] = useState<SymbolColumnEntry[] | null>(null);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<string | null>(null);

  useEffect(() => {
    invoke<SymbolColumnsConfig>("load_symbol_columns_config").then((cfg) =>
      setEntries(cfg.entries),
    );
  }, []);

  function toggle(index: number) {
    if (!entries) return;
    if (MANDATORY.has(entries[index].column)) return;
    setEntries(entries.map((e, i) => (i === index ? { ...e, visible: !e.visible } : e)));
    setStatus(null);
  }

  function move(index: number, dir: -1 | 1) {
    if (!entries) return;
    const target = index + dir;
    if (target < 0 || target >= entries.length) return;
    const next = [...entries];
    [next[index], next[target]] = [next[target], next[index]];
    setEntries(next);
    setStatus(null);
  }

  async function save() {
    if (!entries) return;
    setSaving(true);
    setStatus(null);
    try {
      await invoke("save_symbol_columns_config", { config: { entries } });
      setStatus("Saved.");
    } catch (exc) {
      setStatus(String(exc));
    } finally {
      setSaving(false);
    }
  }

  if (!entries) return <p className="field-hint">Loading…</p>;

  return (
    <>
      <div className="xlsx-col-list">
        {entries.map((entry, i) => {
          const mandatory = MANDATORY.has(entry.column);
          const label = COLUMN_LABEL[entry.column] ?? entry.column;
          return (
            <div key={entry.column} className="xlsx-col-row">
              <input
                type="checkbox"
                checked={entry.visible}
                disabled={mandatory}
                onChange={() => toggle(i)}
                title={mandatory ? "Always included" : undefined}
              />
              <span className="xlsx-col-label">{label}</span>
              <button
                type="button"
                className="btn btn-icon btn-xs"
                onClick={() => move(i, -1)}
                disabled={i === 0}
                aria-label="Move up"
              >
                ▲
              </button>
              <button
                type="button"
                className="btn btn-icon btn-xs"
                onClick={() => move(i, 1)}
                disabled={i === entries.length - 1}
                aria-label="Move down"
              >
                ▼
              </button>
            </div>
          );
        })}
      </div>
      <div className="form-actions">
        <button
          type="button"
          className="btn btn-primary btn-sm"
          onClick={save}
          disabled={saving}
        >
          {saving ? "Saving…" : "Save column preferences"}
        </button>
        {status && <span className="field-hint">{status}</span>}
      </div>
    </>
  );
}
