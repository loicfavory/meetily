import { invoke } from '@tauri-apps/api/core';

export interface AudioDevice {
  /** Exact device ID. Identity only: never shown to the user when a label exists. */
  name: string;
  device_type: 'Input' | 'Output';
  /** Human-readable label. Absent on macOS/Windows. */
  label?: string;
  is_default?: boolean;
}

export interface SelectedDevices {
  micDevice: string | null;
  systemDevice: string | null;
  /** Display-only snapshot of the label at the time of the choice. */
  micLabel?: string | null;
  systemLabel?: string | null;
  /** Legacy choice that could not be migrated to a device ID. */
  micUnresolved?: string | null;
  systemUnresolved?: string | null;
  /** The v1 migration has not run yet: unresolved values are provisional. */
  migrationPending?: boolean;
}

export type DeviceRole = 'microphone' | 'system';

export type InventoryStatus = 'complete' | 'degraded' | 'failed';

export interface RoleInventory {
  status: InventoryStatus;
  error?: string | null;
}

export interface AudioInventory {
  devices: AudioDevice[];
  microphone: RoleInventory;
  system: RoleInventory;
}

export type SelectionStatus =
  | { kind: 'default' }
  | { kind: 'available'; id: string; label: string }
  | { kind: 'unknown'; id: string; label: string; reason: 'loading' | 'degraded' | 'failed'; error?: string }
  | { kind: 'absent'; id: string; label: string }
  | { kind: 'unresolved'; legacy: string };

/** Device-related fields of the persisted recording preferences. */
export interface DevicePreferenceFields {
  preferred_mic_device?: string | null;
  preferred_mic_label?: string | null;
  preferred_system_device?: string | null;
  preferred_system_label?: string | null;
  unresolved_mic_device?: string | null;
  unresolved_system_device?: string | null;
  migration_pending?: boolean;
}

export function selectedDevicesFromPreferences(prefs: DevicePreferenceFields): SelectedDevices {
  return {
    micDevice: prefs.preferred_mic_device ?? null,
    systemDevice: prefs.preferred_system_device ?? null,
    micLabel: prefs.preferred_mic_label ?? null,
    systemLabel: prefs.preferred_system_label ?? null,
    micUnresolved: prefs.unresolved_mic_device ?? null,
    systemUnresolved: prefs.unresolved_system_device ?? null,
    migrationPending: prefs.migration_pending ?? false,
  };
}

const DEFAULT_OPTION_VALUE = 'default:';
export const UNRESOLVED_OPTION_VALUE = 'unresolved:';
const DEVICE_OPTION_PREFIX = 'device:';

export function encodeDeviceOption(id: string | null): string {
  return id === null ? DEFAULT_OPTION_VALUE : `${DEVICE_OPTION_PREFIX}${id}`;
}

/**
 * Returns the device ID, null for the Default option, or undefined when the
 * value is not a recognised option value.
 */
export function decodeDeviceOption(value: string): string | null | undefined {
  if (value === DEFAULT_OPTION_VALUE) return null;
  if (value === UNRESOLVED_OPTION_VALUE) return undefined;
  if (value.startsWith(DEVICE_OPTION_PREFIX)) {
    const id = value.slice(DEVICE_OPTION_PREFIX.length);
    return id.length > 0 ? id : undefined;
  }
  return undefined;
}

export function deviceLabel(device: Pick<AudioDevice, 'name' | 'label'>): string {
  const label = device.label?.trim();
  return label ? label : device.name;
}

export function labelForId(devices: readonly AudioDevice[], id: string | null): string | null {
  if (id === null) return null;
  const match = devices.find((d) => d.name === id);
  return match ? deviceLabel(match) : id;
}

const GENERIC_ROLE_LABEL: Record<DeviceRole, string> = {
  microphone: 'Microphone',
  system: 'System audio',
};

/**
 * Status of one role's saved selection against the enumerated inventory
 * (null while it is loading). Pure: never mutates or resets a selection.
 */
export function selectionStatus(
  role: DeviceRole,
  selected: SelectedDevices,
  inventory: AudioInventory | null,
): SelectionStatus {
  const isMic = role === 'microphone';
  const unresolved = isMic ? selected.micUnresolved : selected.systemUnresolved;
  if (unresolved) {
    // While the migration is pending the legacy text is provisional: show it
    // as unavailable rather than asking the user to re-select.
    return selected.migrationPending
      ? { kind: 'unknown', id: unresolved, label: unresolved, reason: 'loading' }
      : { kind: 'unresolved', legacy: unresolved };
  }

  const id = isMic ? selected.micDevice : selected.systemDevice;
  if (id === null) return { kind: 'default' };

  const snapshot = (isMic ? selected.micLabel : selected.systemLabel)?.trim();
  const fallbackLabel = snapshot ? snapshot : GENERIC_ROLE_LABEL[role];

  if (inventory === null) {
    return { kind: 'unknown', id, label: fallbackLabel, reason: 'loading' };
  }

  const type: AudioDevice['device_type'] = isMic ? 'Input' : 'Output';
  const roleInventory = isMic ? inventory.microphone : inventory.system;
  const roleDevices = inventory.devices.filter((d) => d.device_type === type);
  const match = roleDevices.find((d) => d.name === id);
  const label = match ? (match.label?.trim() || fallbackLabel) : fallbackLabel;

  if (roleInventory.status !== 'complete') {
    return {
      kind: 'unknown',
      id,
      label,
      reason: roleInventory.status,
      ...(roleInventory.error ? { error: roleInventory.error } : {}),
    };
  }
  if (roleDevices.length === 0) {
    return { kind: 'unknown', id, label, reason: 'degraded' };
  }
  return match ? { kind: 'available', id, label } : { kind: 'absent', id, label };
}

const labelCache = new Map<string, string>();

function rememberLabels(devices: readonly AudioDevice[]): void {
  for (const device of devices) {
    labelCache.set(device.name, deviceLabel(device));
  }
}

export async function fetchAudioDevices(): Promise<AudioDevice[]> {
  const devices = await invoke<AudioDevice[]>('get_audio_devices');
  rememberLabels(devices);
  return devices;
}

export async function fetchAudioInventory(): Promise<AudioInventory> {
  const inventory = await invoke<AudioInventory>('get_audio_inventory');
  rememberLabels(inventory.devices);
  return inventory;
}

export function cachedDeviceLabel(id: string): string {
  return labelCache.get(id) ?? id;
}

export async function resolveDeviceLabel(id: string): Promise<string> {
  const cached = labelCache.get(id);
  if (cached !== undefined) return cached;
  try {
    await fetchAudioDevices();
  } catch (err) {
    console.error('Failed to resolve device label:', err);
  }
  return cachedDeviceLabel(id);
}
