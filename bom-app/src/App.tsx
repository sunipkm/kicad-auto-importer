import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { SettingsPanel } from "./SettingsPanel";
import { PopulateBom } from "./PopulateBom";
import { GenerateBom } from "./GenerateBom";
import "./App.css";

// Mirrors the Rust `ProjectInfo` struct in `src-tauri/src/lib.rs`.
interface ProjectInfo {
  project_dir: string;
  root_schematic: string | null;
  placed_symbol_count: number;
}

type Tab = "populate" | "generate";

function App() {
  const [projectDir, setProjectDir] = useState("");
  const [info, setInfo] = useState<ProjectInfo | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("populate");

  async function openProject(path: string) {
    if (!path.trim()) return;
    setError(null);
    try {
      const result = await invoke<ProjectInfo>("open_project", { path });
      setInfo(result);
      setTab("populate");
    } catch (exc) {
      setError(String(exc));
    }
  }

  // Pick a .kicad_pro file, extract its parent directory as the project dir.
  async function browseForProject() {
    const selected = await open({
      filters: [{ name: "KiCad Project", extensions: ["kicad_pro"] }],
      multiple: false,
    });
    if (typeof selected === "string") {
      // Extract parent directory, handling both Unix (/) and Windows (\) path separators.
      const lastSlash = Math.max(
        selected.lastIndexOf("/"),
        selected.lastIndexOf("\\")
      );
      const projectDir = selected.substring(0, lastSlash);
      setProjectDir(projectDir);
      await openProject(projectDir);
    }
  }

  function changeProject() {
    setInfo(null);
    setError(null);
  }

  return (
    <div className="app-shell">
      <header className="app-header">
        <span className="app-title">KiCad BOM Tool</span>

        {info && (
          <div className="app-header-project">
            <span className="project-path" title={info.project_dir}>
              {info.project_dir}
            </span>
            <button type="button" className="btn btn-ghost btn-sm" onClick={changeProject}>
              Change…
            </button>
          </div>
        )}

        <div className="app-header-spacer" />
        <SettingsPanel />
      </header>

      <main className="app-main">
        {!info ? (
          <div className="app-main-centered">
            <div className="card welcome-card">
              <h2>Open a KiCad project</h2>
              <p>
                Select a <code>.kicad_pro</code> file to populate its bill of materials
                with manufacturer/distributor data, or generate a priced BOM
                report.
              </p>
              <form
                className="open-project-row"
                onSubmit={(e) => {
                  e.preventDefault();
                  openProject(projectDir);
                }}
              >
                <input
                  type="text"
                  value={projectDir}
                  onChange={(e) => setProjectDir(e.currentTarget.value)}
                  placeholder="Path to project directory (or browse for .kicad_pro)…"
                />
                <button type="button" className="btn btn-primary" onClick={browseForProject}>
                  Browse…
                </button>
              </form>
              {error && <p className="status-line status-error">{error}</p>}
            </div>
          </div>
        ) : (
          <div className="app-main-project">
            <div className="tabs">
              <button
                type="button"
                className={`tab ${tab === "populate" ? "active" : ""}`}
                onClick={() => setTab("populate")}
              >
                Populate BOM
              </button>
              <button
                type="button"
                className={`tab ${tab === "generate" ? "active" : ""}`}
                onClick={() => setTab("generate")}
              >
                Generate BOM
              </button>
            </div>

            {tab === "populate" && <PopulateBom projectDir={info.project_dir} />}
            {tab === "generate" && <GenerateBom projectDir={info.project_dir} />}
          </div>
        )}
      </main>
    </div>
  );
}

export default App;
