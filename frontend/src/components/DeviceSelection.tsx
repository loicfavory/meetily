import React, { useState, useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { RefreshCw, Mic, Speaker } from 'lucide-react';
import { AudioLevelMeter, CompactAudioLevelMeter } from './AudioLevelMeter';
import { AudioBackendSelector } from './AudioBackendSelector';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Label } from '@/components/ui/label';
import Analytics from '@/lib/analytics';
import {
  AudioDevice,
  AudioInventory,
  DeviceRole,
  RoleInventory,
  SelectedDevices,
  SelectionStatus,
  UNRESOLVED_OPTION_VALUE,
  decodeDeviceOption,
  deviceLabel,
  encodeDeviceOption,
  fetchAudioInventory,
  selectionStatus,
} from '@/lib/audioDevices';

export type { AudioDevice, SelectedDevices };

export interface AudioLevelData {
  device_name: string;
  device_type: string;
  rms_level: number;
  peak_level: number;
  is_active: boolean;
}

export interface AudioLevelUpdate {
  timestamp: number;
  levels: AudioLevelData[];
}

interface DeviceSelectionProps {
  selectedDevices: SelectedDevices;
  onDeviceChange: (devices: SelectedDevices, changedRole: DeviceRole) => void;
  disabled?: boolean;
}

function failedInventory(previous: AudioInventory | null, message: string): AudioInventory {
  const failed: RoleInventory = { status: 'failed', error: message };
  return { devices: previous?.devices ?? [], microphone: failed, system: failed };
}

/**
 * Option shown for a selection that has no entry in the enumerated list, so
 * the Select always has an item matching its value. Null when the list
 * already carries the selection (or the selection is the default).
 */
function placeholderOption(
  status: SelectionStatus,
  listed: readonly AudioDevice[],
): { value: string; text: string } | null {
  switch (status.kind) {
    case 'unknown':
    case 'absent':
      if (listed.some((d) => d.name === status.id)) return null;
      return {
        value: encodeDeviceOption(status.id),
        text: `${status.label} (${status.kind === 'absent' ? 'not connected' : 'unavailable'})`,
      };
    case 'unresolved':
      return { value: UNRESOLVED_OPTION_VALUE, text: `${status.legacy} (select again)` };
    default:
      return null;
  }
}

function selectValue(status: SelectionStatus): string {
  switch (status.kind) {
    case 'default':
      return encodeDeviceOption(null);
    case 'unresolved':
      return UNRESOLVED_OPTION_VALUE;
    default:
      return encodeDeviceOption(status.id);
  }
}

function RoleNotice({ status, inventory }: { status: SelectionStatus; inventory: RoleInventory | null }) {
  let message: string | null = null;
  if (status.kind === 'absent') {
    message = 'The selected device is not connected. Your choice is kept and will be used as soon as it reappears; otherwise the startup fallback applies.';
  } else if (status.kind === 'unresolved') {
    message = 'Your previous choice could not be matched to a device. Select a device again, or choose Default.';
  } else if (status.kind === 'unknown' && status.reason !== 'loading') {
    message = status.error
      ? `Device availability unknown: ${status.error}. Your choice is kept.`
      : 'Device availability unknown. Your choice is kept.';
  } else if (inventory && inventory.status !== 'complete') {
    message = inventory.error
      ? `Device list may be incomplete: ${inventory.error}.`
      : 'Device list may be incomplete.';
  }
  if (!message) return null;
  return <p className="text-xs text-amber-700">{message}</p>;
}

export function DeviceSelection({ selectedDevices, onDeviceChange, disabled = false }: DeviceSelectionProps) {
  const [inventory, setInventory] = useState<AudioInventory | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [audioLevels, setAudioLevels] = useState<Map<string, AudioLevelData>>(new Map());
  const [isMonitoring, setIsMonitoring] = useState(false);
  const [showLevels, setShowLevels] = useState(false);

  const devices = inventory?.devices ?? [];

  // Filter devices by type
  const inputDevices = devices.filter(device => device.device_type === 'Input');
  const outputDevices = devices.filter(device => device.device_type === 'Output');

  const micStatus = selectionStatus('microphone', selectedDevices, inventory);
  const systemStatus = selectionStatus('system', selectedDevices, inventory);

  // Fetch available audio devices. A failed fetch keeps the previous list and
  // marks both roles as failed: the saved selection is never touched here.
  const fetchDevices = async () => {
    try {
      setError(null);
      const result = await fetchAudioInventory();
      setInventory(result);
      console.log('Fetched audio inventory:', result);
    } catch (err) {
      console.error('Failed to fetch audio inventory:', err);
      const message = err instanceof Error ? err.message : String(err);
      setInventory((previous) => failedInventory(previous, message));
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  };

  // Load devices on component mount
  useEffect(() => {
    fetchDevices();
  }, []);

  // Set up audio level event listener
  useEffect(() => {
    let unlisten: (() => void) | undefined;

    const setupAudioLevelListener = async () => {
      try {
        unlisten = await listen<AudioLevelUpdate>('audio-levels', (event) => {
          const levelUpdate = event.payload;
          const newLevels = new Map<string, AudioLevelData>();

          levelUpdate.levels.forEach(level => {
            newLevels.set(level.device_name, level);
          });

          setAudioLevels(newLevels);
        });
      } catch (err) {
        console.error('Failed to setup audio level listener:', err);
      }
    };

    setupAudioLevelListener();

    // Cleanup function
    return () => {
      if (unlisten) {
        unlisten();
      }
      // Stop monitoring when component unmounts
      if (isMonitoring) {
        stopAudioLevelMonitoring();
      }
    };
  }, [isMonitoring]);

  // Handle device refresh
  const handleRefresh = async () => {
    setRefreshing(true);
    await fetchDevices();
  };

  // Helper function to detect device category and Bluetooth status
  const labelFor = (id: string) => {
    const match = devices.find(d => d.name === id);
    return match ? deviceLabel(match) : id;
  };

  const getDeviceMetadata = (deviceName: string | null) => {
    if (deviceName === null) return { isBluetooth: false, category: 'default' };
    const nameLower = deviceName.toLowerCase();

    // Detect if it's Bluetooth
    const isBluetooth = nameLower.includes('airpods')
      || nameLower.includes('bluetooth')
      || nameLower.includes('wireless')
      || nameLower.includes('wh-')  // Sony WH-* series
      || nameLower.includes('bt ');

    // Categorize device
    let category = 'wired';
    if (nameLower.includes('airpods')) {
      category = 'airpods';
    } else if (isBluetooth) {
      category = 'bluetooth';
    }

    return { isBluetooth, category };
  };

  // Handle microphone device selection
  const handleMicDeviceChange = (optionValue: string) => {
    const deviceName = decodeDeviceOption(optionValue);
    if (deviceName === undefined) return;
    const newDevices: SelectedDevices = {
      ...selectedDevices,
      micDevice: deviceName,
      micLabel: deviceName === null ? null : labelFor(deviceName),
      micUnresolved: null,
    };
    onDeviceChange(newDevices, 'microphone');

    // Track device selection analytics with enhanced metadata
    const metadata = getDeviceMetadata(deviceName === null ? null : labelFor(deviceName));
    Analytics.track('microphone_selected', {
      device_category: metadata.category,
      is_bluetooth: metadata.isBluetooth.toString(),
      has_system_audio: (!!selectedDevices.systemDevice).toString()
    }).catch(err => console.error('Failed to track microphone selection:', err));
  };

  // Handle system audio device selection
  const handleSystemDeviceChange = (optionValue: string) => {
    const deviceName = decodeDeviceOption(optionValue);
    if (deviceName === undefined) return;
    const newDevices: SelectedDevices = {
      ...selectedDevices,
      systemDevice: deviceName,
      systemLabel: deviceName === null ? null : labelFor(deviceName),
      systemUnresolved: null,
    };
    onDeviceChange(newDevices, 'system');

    // Track device selection analytics with enhanced metadata
    const metadata = getDeviceMetadata(deviceName === null ? null : labelFor(deviceName));
    Analytics.track('system_audio_selected', {
      device_category: metadata.category,
      is_bluetooth: metadata.isBluetooth.toString(),
      has_microphone: (!!selectedDevices.micDevice).toString()
    }).catch(err => console.error('Failed to track system audio selection:', err));
  };

  // Start audio level monitoring
  const startAudioLevelMonitoring = async () => {
    try {
      // Only monitor input devices for now (microphones)
      const deviceNames = inputDevices.map(device => device.name);
      if (deviceNames.length === 0) {
        setError('No microphone devices found to monitor');
        return;
      }

      await invoke('start_audio_level_monitoring', { deviceNames });
      setIsMonitoring(true);
      setShowLevels(true);
      console.log('Started audio level monitoring for input devices:', deviceNames);
    } catch (err) {
      console.error('Failed to start audio level monitoring:', err);
      setError('Failed to start audio level monitoring');
    }
  };

  // Stop audio level monitoring
  const stopAudioLevelMonitoring = async () => {
    try {
      await invoke('stop_audio_level_monitoring');
      setIsMonitoring(false);
      setAudioLevels(new Map());
      console.log('Stopped audio level monitoring');
    } catch (err) {
      console.error('Failed to stop audio level monitoring:', err);
    }
  };

  // Toggle audio level monitoring
  const toggleAudioLevelMonitoring = async () => {
    if (isMonitoring) {
      await stopAudioLevelMonitoring();
    } else {
      await startAudioLevelMonitoring();
    }
  };

  if (loading) {
    return (
      <div className="p-4 space-y-4">
        <div className="animate-pulse">
          <div className="h-4 bg-gray-200 rounded w-1/3 mb-4"></div>
          <div className="h-10 bg-gray-200 rounded mb-3"></div>
          <div className="h-10 bg-gray-200 rounded"></div>
        </div>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h4 className="text-sm font-medium text-gray-900">Audio Devices</h4>
        <div className="flex items-center space-x-2">
          {/* TODO: Monitoring */}
          {/* <button */}
          {/*   onClick={toggleAudioLevelMonitoring} */}
          {/*   disabled={disabled || inputDevices.length === 0} */}
          {/*   className={`px-3 py-1 text-xs font-medium rounded-md transition-colors ${ */}
          {/*     isMonitoring */}
          {/*       ? 'bg-red-100 text-red-700 hover:bg-red-200' */}
          {/*       : 'bg-green-100 text-green-700 hover:bg-green-200' */}
          {/*   } disabled:pointer-events-none disabled:opacity-50`} */}
          {/*   title={inputDevices.length === 0 ? 'No microphones available to test' : ''} */}
          {/* > */}
          {/*   {isMonitoring ? 'Stop Test' : 'Test Mic'} */}
          {/* </button> */}
          <button
            onClick={handleRefresh}
            disabled={refreshing || disabled}
            className="h-8 w-8 p-0 inline-flex items-center justify-center rounded-md text-sm font-medium transition-colors hover:bg-gray-100 disabled:pointer-events-none disabled:opacity-50"
          >
            <RefreshCw className={`h-4 w-4 ${refreshing ? 'animate-spin' : ''}`} />
          </button>
        </div>
      </div>

      {error && (
        <div className="p-3 text-sm text-red-700 bg-red-50 border border-red-200 rounded-md">
          {error}
        </div>
      )}

      <div className="space-y-3">
        {/* Microphone Selection */}
        <div className="space-y-2">
          <div className="flex items-center gap-2">
            <Mic className="h-4 w-4 text-gray-600" />
            <Label htmlFor="mic-selection" className="text-sm font-medium text-gray-700">
              Microphone
            </Label>
          </div>
          <Select
            value={selectValue(micStatus)}
            onValueChange={handleMicDeviceChange}
            disabled={disabled}
          >
            <SelectTrigger id="mic-selection" className="w-full">
              <SelectValue placeholder="Select Microphone" />
            </SelectTrigger>
            <SelectContent>
              {(() => {
                const extra = placeholderOption(micStatus, inputDevices);
                return extra && <SelectItem value={extra.value} disabled>{extra.text}</SelectItem>;
              })()}
              <SelectItem value={encodeDeviceOption(null)}>Default Microphone</SelectItem>
              {inputDevices.map((device) => (
                <SelectItem
                  key={device.name}
                  value={encodeDeviceOption(device.name)}
                >
                  {deviceLabel(device)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
          {inputDevices.length === 0 && (
            <p className="text-xs text-gray-500">No microphone devices found</p>
          )}
          <RoleNotice status={micStatus} inventory={inventory?.microphone ?? null} />

          {/* Audio Level Meters for Input Devices */}
          {showLevels && inputDevices.length > 0 && (
            <div className="space-y-2 pt-2 border-t border-gray-100">
              <p className="text-xs text-gray-600 font-medium">Microphone Levels:</p>
              {inputDevices.map((device) => {
                const levelData = audioLevels.get(device.name);
                return (
                  <div key={`level-${device.name}`} className="space-y-1">
                    <div className="flex items-center justify-between">
                      <span className="text-xs text-gray-600 truncate max-w-[200px]">
                        {deviceLabel(device)}
                      </span>
                      {levelData && (
                        <CompactAudioLevelMeter
                          rmsLevel={levelData.rms_level}
                          peakLevel={levelData.peak_level}
                          isActive={levelData.is_active}
                        />
                      )}
                    </div>
                    {levelData && (
                      <AudioLevelMeter
                        rmsLevel={levelData.rms_level}
                        peakLevel={levelData.peak_level}
                        isActive={levelData.is_active}
                        deviceName={deviceLabel(device)}
                        size="small"
                      />
                    )}
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {/* System Audio Selection */}
        <div className="space-y-2">
          <div className="flex items-center gap-2">
            <Speaker className="h-4 w-4 text-gray-600" />
            <Label htmlFor="system-selection" className="text-sm font-medium text-gray-700">
              System Audio
            </Label>
          </div>

          <Select
            value={selectValue(systemStatus)}
            onValueChange={handleSystemDeviceChange}
            disabled={disabled}
          >
            <SelectTrigger id="system-selection" className="w-full">
              <SelectValue placeholder="Select System Audio" />
            </SelectTrigger>
            <SelectContent>
              {(() => {
                const extra = placeholderOption(systemStatus, outputDevices);
                return extra && <SelectItem value={extra.value} disabled>{extra.text}</SelectItem>;
              })()}
              <SelectItem value={encodeDeviceOption(null)}>Default System Audio</SelectItem>
              {outputDevices.map((device) => (
                <SelectItem
                  key={device.name}
                  value={encodeDeviceOption(device.name)}
                >
                  {deviceLabel(device)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>

          {outputDevices.length === 0 && (
            <p className="text-xs text-gray-500">No system audio devices found</p>
          )}
          <RoleNotice status={systemStatus} inventory={inventory?.system ?? null} />

          {/* Backend Selection - available on all platforms */}
          {!disabled && (
            <div className="pt-3 border-t border-gray-100">
              <AudioBackendSelector disabled={disabled} />
            </div>
          )}
        </div>
      </div>

      {/* Info text */}
      <div className="text-xs text-gray-500 space-y-1">
        <p>• <strong>Microphone:</strong> Records your voice and ambient sound</p>
        <p>• <strong>System Audio:</strong> Records computer audio (music, calls, etc.)</p>
        {isMonitoring && (
          <p>• <strong>Mic Levels:</strong> Green = good, Yellow = loud, Red = too loud</p>
        )}
        {!isMonitoring && inputDevices.length > 0 && (
          <p>• <strong>Tip:</strong> Click "Test Mic" to check if your microphone is working</p>
        )}
      </div>
    </div>
  );
}
