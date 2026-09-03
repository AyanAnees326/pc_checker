import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { motion, useReducedMotion } from "framer-motion";
import { CpuSection } from "./cpu/CpuSection";
import { GpuStressCard } from "./stress/GpuStressCard";
import { BatterySection } from "./battery/BatterySection";
import { StorageSection } from "./storage/StorageSection";
import { MemorySection } from "./memory/MemorySection";
import { TelemetryFullscreenContext } from "./ui";

// Identity, Graphics (adapter-identity-only) and Stability sections exist and their
// backend commands still work — they're just not part of the rendered card set right
// now, per an explicit "only CPU/GPU/RAM/Storage/Battery, no other cards" request.
// See `src/identity/`, `src/graphics/`, `src/stability/` to bring one back.

export default function App() {
  const [elevated, setElevated] = useState<boolean | null>(null);
  const [graphsContainer, setGraphsContainer] = useState<HTMLDivElement | null>(null);
  const [telemetryFullscreen, setTelemetryFullscreen] = useState(false);
  const reducedMotion = useReducedMotion();

  // Elevation is a cheap OS query, not a hardware probe, so it is fetched once on
  // load rather than gated behind its own scan button.
  useEffect(() => {
    invoke<boolean>("is_elevated")
      .then(setElevated)
      .catch(() => setElevated(null));
  }, []);

  // Esc closes fullscreen telemetry, and body scroll is locked while it's open so the
  // card grid behind the fixed overlay can't be scrolled underneath it.
  useEffect(() => {
    if (!telemetryFullscreen) return;
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape") setTelemetryFullscreen(false);
    };
    document.addEventListener("keydown", onKeyDown);
    const prevOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.removeEventListener("keydown", onKeyDown);
      document.body.style.overflow = prevOverflow;
    };
  }, [telemetryFullscreen]);

  return (
    <TelemetryFullscreenContext.Provider value={telemetryFullscreen}>
      <main className="max-w-[1400px] mx-auto px-6 pt-8 pb-20">
        <header className="flex justify-between items-start gap-4 mb-7">
          <div>
            <motion.h1
              className="text-gradient text-2xl tracking-[-0.02em]"
              initial={reducedMotion ? false : { opacity: 0, y: -8 }}
              animate={{ opacity: 1, y: 0 }}
              transition={{ duration: 0.5, ease: "easeOut" }}
            >
              PC Checker
            </motion.h1>
            <p className="mt-1 text-muted text-sm">
              Inspect a used machine before you pay for it — scan each part on its own.
            </p>
          </div>
          {elevated === false && (
            <span
              className="badge-watch inline-block px-2 py-0.5 rounded-full text-[0.7rem] font-bold uppercase tracking-wide cursor-default"
              title="Relaunch as administrator to unlock CPU/GPU sensor readings during stress tests"
            >
              Not elevated
            </span>
          )}
        </header>

        <div className="columns-1 sm:columns-2 lg:columns-3 xl:columns-4 gap-5">
          <div className="mb-5 break-inside-avoid">
            <CpuSection graphsContainer={graphsContainer} />
          </div>
          <div className="mb-5 break-inside-avoid">
            <GpuStressCard graphsContainer={graphsContainer} />
          </div>
          <div className="mb-5 break-inside-avoid">
            <BatterySection graphsContainer={graphsContainer} />
          </div>
          <div className="mb-5 break-inside-avoid">
            <StorageSection />
          </div>
          <div className="mb-5 break-inside-avoid">
            <MemorySection />
          </div>
        </div>

        <section
          className={
            telemetryFullscreen
              ? "fixed inset-0 z-50 overflow-y-auto bg-bg p-8"
              : "glass-card rounded-lg p-6 mt-5"
          }
        >
          <div className="flex items-center gap-3 mb-1 flex-wrap">
            <h2 className="m-0 text-[0.95rem] uppercase tracking-[0.06em] text-text">Live telemetry</h2>
            <button
              type="button"
              onClick={() => setTelemetryFullscreen((f) => !f)}
              className="ml-auto bg-transparent border border-border rounded-md px-2.5 py-1 text-xs text-muted hover:text-text hover:border-border-glow cursor-pointer transition-colors"
            >
              {telemetryFullscreen ? "✕ Close" : "⛶ Fullscreen"}
            </button>
          </div>
          <p className="text-muted text-sm mb-4">
            Clock, power, temperature and capacity trends from any running scan or stress test
          </p>
          <div ref={setGraphsContainer} className="flex flex-col gap-6" />
        </section>
      </main>
    </TelemetryFullscreenContext.Provider>
  );
}
