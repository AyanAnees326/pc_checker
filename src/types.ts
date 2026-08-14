// Mirrors the Rust types in src-tauri/src/model.rs and probes/.

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

export function readingValue<T>(r: Reading<T> | undefined): T | null {
  return r && r.ok ? r.value : null;
}

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
  acpi_tables: string[];
  absolute_agent_artifacts: string[];
}

export interface FirmwareReport {
  identity: SystemIdentity;
  persistence: FirmwarePersistence;
}

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

export interface NvmeHealth {
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

export interface DriveReport {
  index: number;
  model: Reading<string>;
  serial: Reading<string>;
  firmware: Reading<string>;
  bus_type: string;
  removable: boolean;
  health: Reading<NvmeHealth>;
}

export interface Inventory {
  firmware: FirmwareReport;
  batteries: Reading<BatteryReport[]>;
  battery_history: Reading<BatteryHistory>;
  memory: MemoryReport;
  drives: DriveReport[];
  elevated: boolean;
  collected_at: string;
}
