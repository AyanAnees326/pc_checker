// Mirrors the Rust types in src-tauri/src/model.rs, probes/ and analysis/.

export type Unavailable = {
  kind:
    | "not_supported_by_hardware"
    | "requires_elevation"
    | "driver_missing"
    | "vendor_library_missing"
    | "query_failed"
    | "implausible_value"
    | "not_applicable";
  detail?: string;
};

/**
 * A metric that may legitimately be unreadable on a given machine.
 * Always branch on `ok` — never coerce a missing reading to 0.
 */
export type Reading<T> =
  | { ok: true; value: T }
  | { ok: false; reason: Unavailable; note: string };

/** The value, or `null` when the reading is missing. Never invents a default. */
export function readingValue<T>(r: Reading<T> | undefined): T | null {
  return r && r.ok ? r.value : null;
}

/** The reason a reading is missing, for display next to an empty field. */
export function readingNote(r: Reading<unknown> | undefined): string | null {
  return r && !r.ok ? r.note : null;
}

// --- Findings ---------------------------------------------------------------

export type Severity = "ok" | "watch" | "problem" | "critical";
export type Confidence = "spec_grounded" | "cohort_grounded" | "heuristic";

export interface Finding {
  id: string;
  title: string;
  severity: Severity;
  confidence: Confidence;
  observed: string;
  expected: string;
  basis: string;
  recommendation: string;
  estimated_cost_usd?: number;
}

// --- Battery ----------------------------------------------------------------

export type PowerState =
  | "charging"
  | "discharging"
  | "on_line_not_charging"
  | "critical"
  | "unknown";

export interface BatteryReport {
  device_name: Reading<string>;
  manufacturer: Reading<string>;
  serial_number: Reading<string>;
  unique_id: Reading<string>;
  manufacture_date: Reading<string>;
  chemistry: Reading<string>;
  power_state: Reading<PowerState>;
  current_capacity_percent: Reading<number>;
  current_capacity_mwh: Reading<number>;
  voltage_mv: Reading<number>;
  charge_rate_mw: Reading<number>;
  temperature_c: Reading<number>;
  full_charged_capacity_mwh: Reading<number>;
  designed_capacity_mwh: Reading<number>;
  health_percent: Reading<number>;
  cycle_count: Reading<number>;
  low_battery_capacity_1: Reading<number>;
  low_battery_capacity_2: Reading<number>;
  critical_bias: Reading<number>;
  estimated_runtime_s: Reading<number>;
  full_runtime_s: Reading<number>;
  capacity_is_relative: boolean;
  is_system_battery: boolean;
}

export interface CapacitySample {
  period_start: string;
  period_end: string;
  design_capacity_mwh: number;
  full_charge_capacity_mwh: number;
  health_percent: number;
  cycle_count: number | null;
  battery_changed: boolean;
}

export interface BatteryHistory {
  samples: CapacitySample[];
  battery_swap_detected: boolean;
  health_first: Reading<number>;
  health_last: Reading<number>;
  degradation_percent_per_year: Reading<number>;
  observation_days: number;
}

// --- Firmware ---------------------------------------------------------------

export type FormFactor =
  | "laptop"
  | "desktop"
  | "all_in_one"
  | "tablet"
  | "server"
  | "unknown";

export interface SystemIdentity {
  manufacturer: Reading<string>;
  product_name: Reading<string>;
  version: Reading<string>;
  serial_number: Reading<string>;
  sku: Reading<string>;
  family: Reading<string>;
  uuid: Reading<string>;
  baseboard_manufacturer: Reading<string>;
  baseboard_product: Reading<string>;
  baseboard_serial: Reading<string>;
  bios_vendor: Reading<string>;
  bios_version: Reading<string>;
  bios_release_date: Reading<string>;
  form_factor: FormFactor;
  chassis_type_code: Reading<number>;
  smbios_version: Reading<string>;
}

export interface FirmwarePersistence {
  wpbt_present: boolean;
  wpbt_launcher_present: boolean;
  acpi_tables: string[];
  absolute_agent_artifacts: string[];
}

export interface FirmwareReport {
  identity: SystemIdentity;
  persistence: FirmwarePersistence;
}

// --- Memory -----------------------------------------------------------------

export type ChannelConfig =
  | "single"
  | "dual"
  | "quad"
  | "asymmetric"
  | "unknown";

export interface MemoryModule {
  slot: Reading<string>;
  bank: Reading<string>;
  size_mb: number;
  memory_type: string;
  form_factor: string;
  soldered: boolean;
  rated_speed_mts: Reading<number>;
  configured_speed_mts: Reading<number>;
  manufacturer: Reading<string>;
  part_number: Reading<string>;
  serial: Reading<string>;
}

export interface MemoryReport {
  modules: MemoryModule[];
  total_mb: number;
  populated_slots: number;
  total_slots: number;
  channel_config: ChannelConfig;
  running_below_rated_speed: boolean;
  all_soldered: boolean;
  mismatched_modules: boolean;
}

// --- Storage ----------------------------------------------------------------

export interface NvmeHealth {
  protocol: "nvme";
  critical_warning: number;
  composite_temp_c: Reading<number>;
  available_spare_percent: Reading<number>;
  available_spare_threshold_percent: Reading<number>;
  percentage_used: Reading<number>;
  power_on_hours: Reading<number>;
  power_cycles: Reading<number>;
  unsafe_shutdowns: Reading<number>;
  media_errors: Reading<number>;
  error_log_entries: Reading<number>;
  data_units_read: Reading<number>;
  data_units_written: Reading<number>;
  terabytes_written: Reading<number>;
}

export interface SmartAttribute {
  id: number;
  name: string;
  current: number;
  worst: number;
  raw: number;
}

export interface AtaHealth {
  protocol: "ata";
  power_on_hours: Reading<number>;
  power_cycles: Reading<number>;
  reallocated_sectors: Reading<number>;
  pending_sectors: Reading<number>;
  uncorrectable_sectors: Reading<number>;
  temperature_c: Reading<number>;
  life_remaining_percent: Reading<number>;
  terabytes_written: Reading<number>;
  attributes: SmartAttribute[];
}

export type DriveHealth = NvmeHealth | AtaHealth;

export interface DriveReport {
  index: number;
  model: Reading<string>;
  serial: Reading<string>;
  firmware: Reading<string>;
  bus_type: string;
  removable: boolean;
  health: Reading<DriveHealth>;
}

// --- GPU --------------------------------------------------------------------

export interface GpuReport {
  name: string;
  vendor: string;
  vendor_id: number;
  device_id: number;
  subsystem_id: number;
  revision: number;
  dedicated_vram_bytes: number;
  shared_memory_bytes: number;
  is_software: boolean;
  has_output: boolean;
  driver_version: Reading<string>;
  driver_date: Reading<string>;
}

// --- Crashes ----------------------------------------------------------------

export interface CrashEvent {
  timestamp: string;
  source: string;
  event_id: number;
  level: string;
}

export interface CrashHistory {
  minidump_count: number;
  most_recent_minidump: Reading<string>;
  minidumps_last_30_days: number;
  full_memory_dump_present: boolean;
  whea_events: Reading<CrashEvent[]>;
  whea_uncorrected_count: number;
  unexpected_shutdowns: Reading<CrashEvent[]>;
}

// --- Top level --------------------------------------------------------------

export interface Inventory {
  firmware: FirmwareReport;
  batteries: Reading<BatteryReport[]>;
  battery_history: Reading<BatteryHistory>;
  memory: MemoryReport;
  drives: DriveReport[];
  gpus: Reading<GpuReport[]>;
  crashes: CrashHistory;
  elevated: boolean;
  collected_at: string;
}

export interface ScanResult {
  inventory: Inventory;
  findings: Finding[];
}

// --- CPU stress test ---------------------------------------------------------

export type CpuVendor = "intel" | "amd" | "other";

export interface CpuTopology {
  vendor: CpuVendor;
  brand_string: Reading<string>;
  physical_cores: Reading<number>;
  logical_processors: number;
  base_clock_mhz: Reading<number>;
}

export type StressPhase = "idle_baseline" | "single_thread_boost" | "all_core_sustained" | "cooldown";

export const STRESS_PHASES: StressPhase[] = [
  "idle_baseline",
  "single_thread_boost",
  "all_core_sustained",
  "cooldown",
];

export const PHASE_LABEL: Record<StressPhase, string> = {
  idle_baseline: "Idle baseline",
  single_thread_boost: "Single-thread boost",
  all_core_sustained: "All-core sustained",
  cooldown: "Cooldown",
};

export interface CpuSample {
  elapsed_ms: number;
  phase: StressPhase;
  effective_clock_mhz: Reading<number>;
  package_power_watts: Reading<number>;
  configured_pl1_watts: Reading<number>;
  configured_pl2_watts: Reading<number>;
  package_temperature_c: Reading<number>;
  thermal_throttling: Reading<boolean>;
  self_check_ok: boolean | null;
  total_iterations: number;
}

export interface CpuStressResult {
  samples: CpuSample[];
  aborted: boolean;
  abort_reason: string | null;
  self_check_failed: boolean;
}

export interface StressStartedEvent {
  topology: CpuTopology;
}

/** One sample from the standalone live CPU monitor — same telemetry as `CpuSample`,
 * reviewable at ~1 Hz without running the full stress test. */
export interface CpuLiveSample {
  elapsed_ms: number;
  effective_clock_mhz: Reading<number>;
  package_power_watts: Reading<number>;
  package_temperature_c: Reading<number>;
  thermal_throttling: Reading<boolean>;
}

// --- GPU stress test ----------------------------------------------------------

export type GpuStressPhase = "idle_baseline" | "compute_sustained" | "cooldown";

export const GPU_STRESS_PHASES: GpuStressPhase[] = ["idle_baseline", "compute_sustained", "cooldown"];

export const GPU_PHASE_LABEL: Record<GpuStressPhase, string> = {
  idle_baseline: "Idle baseline",
  compute_sustained: "Compute sustained",
  cooldown: "Cooldown",
};

export interface GpuSample {
  elapsed_ms: number;
  phase: GpuStressPhase;
  graphics_clock_mhz: Reading<number>;
  power_watts: Reading<number>;
  /** "GPU Temperature" on AMD; NVML's single sensor on NVIDIA. */
  edge_temperature_c: Reading<number>;
  /** Junction/die temperature. AMD-only. */
  hotspot_temperature_c: Reading<number>;
  /** AMD-only — no RPM equivalent in NVIDIA's public NVML API. */
  fan_rpm: Reading<number>;
  fan_percent: Reading<number>;
  sw_thermal_slowdown: Reading<boolean>;
  hw_thermal_slowdown: Reading<boolean>;
  sw_power_cap: Reading<boolean>;
  hw_power_brake: Reading<boolean>;
  self_check_ok: boolean | null;
  /** Compute dispatches submitted, each over the full 64 MiB working set. Correctness
   *  is reported separately via `self_check_ok` — verification is sampled rather than
   *  run per dispatch, because verifying every one is what starved the GPU. */
  dispatches_completed: number;
}

export interface GpuStressResult {
  samples: GpuSample[];
  aborted: boolean;
  abort_reason: string | null;
  self_check_failed: boolean;
}

export interface GpuStressStartedEvent {
  gpu: GpuReport;
}
