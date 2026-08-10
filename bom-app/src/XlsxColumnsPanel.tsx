import { useEffect, useState, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";

type StandardColumn =
  | "Part"
  | "References"
  | "NeededQty"
  | "PurchaseQty"
  | "Vendor"
  | "UnitPrice"
  | "TotalPrice"
  | "InStock"
  | "StockQty"
  | "StockShortfall"
  | "LifecycleConcern";

type XlsxColumnKey =
  | { type: "standard"; value: StandardColumn }
  | { type: "custom"; value: string };

interface XlsxColumnEntry {
  column: XlsxColumnKey;
  visible: boolean;
}

interface XlsxColumnsConfig {
  entries: XlsxColumnEntry[];
}

interface ColumnProfile {
  name: string;
  entries: XlsxColumnEntry[];
}

const COLUMN_LABEL: Record<StandardColumn, string> = {
  Part: "Part",
  References: "References",
  NeededQty: "Need",
  PurchaseQty: "Buy",
  Vendor: "Vendor",
  UnitPrice: "Unit Price",
  TotalPrice: "Ext Price",
  InStock: "In Stock",
  StockQty: "Stock Qty",
  StockShortfall: "Stock Shortfall",
  LifecycleConcern: "Lifecycle Concern",
};

function getColumnLabel(col: XlsxColumnKey): string {
  if (col.type === "standard") {
    return COLUMN_LABEL[col.value];
  }
  return col.value;
}

function getColumnId(col: XlsxColumnKey): string {
  if (col.type === "standard") {
    return col.value;
  }
  return `custom-${col.value}`;
}

const MANDATORY: Set<string> = new Set(["Part", "References", "NeededQty"]);

export function XlsxColumnsPanel() {
  const [entries, setEntries] = useState<XlsxColumnEntry[] | null>(null);
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [draggedIndex, setDraggedIndex] = useState<number | null>(null);
  const [hoverIndex, setHoverIndex] = useState<number | null>(null);
  const [profiles, setProfiles] = useState<ColumnProfile[]>([]);
  const [showProfileForm, setShowProfileForm] = useState(false);
  const [profileName, setProfileName] = useState("");

  const dragStartRef = useRef<{ index: number; startY: number } | null>(null);

  useEffect(() => {
    invoke<XlsxColumnsConfig>("load_xlsx_columns_config").then((cfg) =>
      setEntries(cfg.entries),
    );
    invoke<{ profiles: ColumnProfile[] }>("load_column_profiles").then((data) =>
      setProfiles(data.profiles),
    ).catch(() => setProfiles([]));
  }, []);

  function toggle(index: number) {
    if (!entries) return;
    const col = entries[index].column;
    const isMandatory = col.type === "standard" && MANDATORY.has(col.value);
    if (isMandatory) return;
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

  function handleMouseDown(e: React.MouseEvent, index: number) {
    // Only start drag from the row itself, not from interactive elements
    if ((e.target as HTMLElement).tagName === "INPUT" ||
        (e.target as HTMLElement).tagName === "BUTTON") {
      return;
    }
    e.preventDefault();
    dragStartRef.current = { index, startY: e.clientY };
    setDraggedIndex(index);
  }

  function handleMouseMove(e: React.MouseEvent) {
    if (dragStartRef.current === null) return;

    const threshold = 5; // pixels
    const distance = Math.abs(e.clientY - dragStartRef.current.startY);

    if (distance > threshold) {
      // We're in a drag
      (e.currentTarget as HTMLElement).style.cursor = "grabbing";
    }
  }

  function handleMouseEnter(index: number) {
    if (dragStartRef.current !== null) {
      setHoverIndex(index);
    }
  }


  function handleMouseUp(dropIndex: number) {
    if (dragStartRef.current === null) return;

    const draggedIdx = dragStartRef.current.index;
    dragStartRef.current = null;
    setDraggedIndex(null);

    if (draggedIdx === dropIndex || !entries) {
      setHoverIndex(null);
      return;
    }

    // Perform the reorder
    const next = [...entries];
    const [draggedItem] = next.splice(draggedIdx, 1);
    next.splice(dropIndex, 0, draggedItem);
    setEntries(next);
    setHoverIndex(null);
    setStatus(null);
  }

  async function save() {
    if (!entries) return;
    setSaving(true);
    setStatus(null);
    try {
      await invoke("save_xlsx_columns_config", { config: { entries } });
      setStatus("Saved.");
    } catch (exc) {
      setStatus(String(exc));
    } finally {
      setSaving(false);
    }
  }

  async function saveProfile() {
    if (!profileName.trim() || !entries) return;
    setSaving(true);
    setStatus(null);
    try {
      const newProfile: ColumnProfile = { name: profileName.trim(), entries };
      const updated = [...profiles.filter(p => p.name !== profileName.trim()), newProfile];
      await invoke("save_column_profiles", { profiles: updated });
      setProfiles(updated);
      setProfileName("");
      setShowProfileForm(false);
      setStatus("Profile saved.");
    } catch (exc) {
      setStatus(`Failed to save profile: ${exc}`);
    } finally {
      setSaving(false);
    }
  }

  async function loadProfile(profile: ColumnProfile) {
    setEntries(profile.entries);
    setStatus(`Loaded profile: ${profile.name}`);
  }

  async function deleteProfile(name: string) {
    setSaving(true);
    setStatus(null);
    try {
      const updated = profiles.filter(p => p.name !== name);
      await invoke("save_column_profiles", { profiles: updated });
      setProfiles(updated);
      setStatus("Profile deleted.");
    } catch (exc) {
      setStatus(`Failed to delete profile: ${exc}`);
    } finally {
      setSaving(false);
    }
  }

  if (!entries) return <p className="field-hint">Loading…</p>;

  return (
    <>
      <div className="xlsx-col-list">
        {entries.map((entry, i) => {
          const isMandatory = entry.column.type === "standard" && MANDATORY.has(entry.column.value);
          const label = getColumnLabel(entry.column);
          const columnId = getColumnId(entry.column);
          const isDragged = draggedIndex === i;
          const isHovered = hoverIndex === i;

          return (
            <div
              key={columnId}
              className={`xlsx-col-row${isDragged ? " dragging" : ""}${isHovered ? " drop-target" : ""}`}
              onMouseDown={(e) => handleMouseDown(e, i)}
              onMouseMove={handleMouseMove}
              onMouseEnter={() => handleMouseEnter(i)}
              onMouseUp={() => handleMouseUp(i)}
            >
              <input
                type="checkbox"
                checked={entry.visible}
                disabled={isMandatory}
                onChange={() => toggle(i)}
                title={isMandatory ? "Always included" : undefined}
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
          {saving ? "Saving…" : "Save column order"}
        </button>
        <button
          type="button"
          className="btn btn-secondary btn-sm"
          onClick={() => setShowProfileForm(!showProfileForm)}
          disabled={saving}
        >
          {showProfileForm ? "Cancel" : "Save as Profile"}
        </button>
        {status && <span className="field-hint">{status}</span>}
      </div>

      {showProfileForm && (
        <div style={{ marginTop: "0.5rem", padding: "0.5rem", border: "1px solid var(--border)", borderRadius: "4px" }}>
          <label style={{ display: "block", marginBottom: "0.5rem" }}>
            Profile name:
            <input
              type="text"
              value={profileName}
              onChange={(e) => setProfileName(e.currentTarget.value)}
              placeholder="e.g., Production, Development"
              onKeyDown={(e) => {
                if (e.key === "Enter") saveProfile();
                if (e.key === "Escape") setShowProfileForm(false);
              }}
              style={{ marginLeft: "0.5rem", width: "200px" }}
            />
          </label>
          <button
            type="button"
            className="btn btn-primary btn-xs"
            onClick={saveProfile}
            disabled={saving || !profileName.trim()}
          >
            Save Profile
          </button>
        </div>
      )}

      {profiles.length > 0 && (
        <div style={{ marginTop: "0.75rem", padding: "0.5rem", border: "1px solid var(--border)", borderRadius: "4px" }}>
          <p style={{ margin: "0 0 0.5rem 0", fontSize: "0.85rem", fontWeight: "500" }}>Saved Profiles:</p>
          <div style={{ display: "flex", flexWrap: "wrap", gap: "0.25rem" }}>
            {profiles.map((profile) => (
              <div
                key={profile.name}
                style={{
                  display: "flex",
                  alignItems: "center",
                  gap: "0.25rem",
                  padding: "0.25rem 0.5rem",
                  backgroundColor: "var(--surface-alt)",
                  borderRadius: "3px",
                  fontSize: "0.85rem",
                }}
              >
                <button
                  type="button"
                  className="btn-link"
                  onClick={() => loadProfile(profile)}
                  disabled={saving}
                  style={{ padding: 0, textDecoration: "underline", cursor: "pointer" }}
                >
                  {profile.name}
                </button>
                <button
                  type="button"
                  className="btn-icon btn-xs"
                  onClick={() => deleteProfile(profile.name)}
                  disabled={saving}
                  title="Delete profile"
                  style={{ padding: 0 }}
                >
                  ✕
                </button>
              </div>
            ))}
          </div>
        </div>
      )}
    </>
  );
}
