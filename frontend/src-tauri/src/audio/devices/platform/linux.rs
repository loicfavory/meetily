use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait};
use log::{info, warn};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::audio::capture::{
    default_pulse_sink_name, default_pulse_source_name, list_pulse_sinks, list_pulse_sources,
    PulseSink, PulseSource,
};
use crate::audio::devices::configuration::{
    AudioDevice, DeviceType, InventoryStatus, RoleInventory,
};
use crate::audio::recording_preferences::{Candidate, MigrationInventory, RoleCandidates};

/// Per-role completeness of the last enumeration.
#[derive(Debug, Clone, PartialEq)]
pub struct LinuxInventoryStatus {
    pub microphone: RoleInventory,
    pub system: RoleInventory,
}

/// Maps the outcome of the two Pulse enumerations to per-role statuses. A
/// failed source listing is `Degraded` rather than `Failed` because the ALSA
/// fallback still yields a list, but one that cannot prove a Pulse ID absent.
pub(crate) fn inventory_status(
    sources: Result<(), String>,
    sinks: Result<(), String>,
) -> LinuxInventoryStatus {
    let role = |result: Result<(), String>, on_error: InventoryStatus| match result {
        Ok(()) => RoleInventory::complete(),
        Err(error) => RoleInventory::with_error(on_error, error),
    };
    LinuxInventoryStatus {
        microphone: role(sources, InventoryStatus::Degraded),
        system: role(sinks, InventoryStatus::Failed),
    }
}

/// Raw Pulse descriptions per role, as needed to migrate legacy preferences.
/// Goes through the existing enumerator functions; a failing call makes that
/// role `Unknown`.
pub(crate) fn migration_inventory() -> MigrationInventory {
    let microphone = match list_pulse_sources() {
        Ok(sources) => {
            let entries: Vec<(&str, &str)> = sources
                .iter()
                .map(|s| (s.source_name.as_str(), s.description.as_str()))
                .collect();
            let labels = role_labels(&entries, GENERIC_INPUT_LABEL);
            RoleCandidates::Known(
                sources
                    .iter()
                    .zip(labels)
                    .map(|(s, label)| Candidate {
                        id: s.source_name.clone(),
                        description: s.description.clone(),
                        label,
                    })
                    .collect(),
            )
        }
        Err(e) => {
            warn!("migration_inventory: failed to list sources: {}", e);
            RoleCandidates::Unknown
        }
    };
    let system = match list_pulse_sinks() {
        Ok(sinks) => {
            let entries: Vec<(&str, &str)> = sinks
                .iter()
                .map(|s| (s.name.as_str(), s.description.as_str()))
                .collect();
            let labels = role_labels(&entries, GENERIC_OUTPUT_LABEL);
            RoleCandidates::Known(
                sinks
                    .iter()
                    .zip(labels)
                    .map(|(s, label)| Candidate {
                        id: s.name.clone(),
                        description: s.description.clone(),
                        label,
                    })
                    .collect(),
            )
        }
        Err(e) => {
            warn!("migration_inventory: failed to list sinks: {}", e);
            RoleCandidates::Unknown
        }
    };
    MigrationInventory { microphone, system }
}

/// Global lock serializing every interaction with alsa-lib on Linux.
///
/// alsa-lib's config/cache functions (`snd_device_name_hint`,
/// `snd_config_update_r`) and PCM open functions (`snd_pcm_open`, called
/// internally by `default_input_config`/`default_output_config`/
/// `build_input_stream`) all mutate a process-global config cache and are not
/// safe to call concurrently from multiple threads. `list_audio_devices()` is
/// polled every few seconds by the device disconnect/reconnect monitor
/// (device_monitor.rs) while a recording is active, and PCM setup happens on
/// another Tokio worker thread whenever a cpal fallback stream starts
/// (degraded mode, stale saved preference, permission trigger, etc.). Without
/// serialization, an overlapping call corrupts alsa-lib's heap state, which
/// glibc eventually detects and aborts the process on — this is what produced
/// the SIGABRT crashes after ~30 minutes of recording.
pub(crate) static ALSA_GLOBAL_LOCK: Mutex<()> = Mutex::new(());

/// Degraded-mode filter: which raw cpal/ALSA PCM names are worth showing
/// when the PulseAudio/PipeWire server can't be reached.
///
/// A whitelist is deliberately used instead of a blacklist: ALSA exposes many
/// plugins, rate converters, and user-defined aliases (~/.asoundrc) that all
/// look like plausible devices. Blacklisting them is endless and still leaves
/// the list confusing. The whitelist gives a bounded, predictable fallback.
pub(crate) fn is_useful_alsa_endpoint(name: &str) -> bool {
    matches!(name.to_lowercase().as_str(), "default" | "pipewire" | "pulse")
}

/// Last seen display label per device ID, so events about a device that has
/// just disappeared can still show its readable name.
static LABEL_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn remember_labels(devices: &[AudioDevice]) {
    let mut cache = LABEL_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    for device in devices {
        if let Some(label) = &device.label {
            map.insert(device.name.clone(), label.clone());
        }
    }
}

/// Display label for a device ID; the ID itself when no label was seen yet.
pub fn cached_label(id: &str) -> String {
    let cache = LABEL_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .as_ref()
        .and_then(|map| map.get(id).cloned())
        .unwrap_or_else(|| id.to_string())
}

const GENERIC_INPUT_LABEL: &str = "Microphone";
const GENERIC_OUTPUT_LABEL: &str = "Sortie audio";

/// Short, human-scannable form of a PulseAudio name, used only to tell apart
/// identical labels: drops the `alsa_input.` / `bluez_output.` style prefix.
fn short_id(id: &str) -> &str {
    match id.split_once('.') {
        Some((_, rest)) if !rest.is_empty() => rest,
        _ => id,
    }
}

/// Display labels for one role (inputs or outputs). Empty descriptions become
/// the generic role label, never the raw ID; labels that collide within the
/// role get the short ID appended. The suffix is display-only and never part
/// of an identity.
fn role_labels(entries: &[(&str, &str)], generic: &str) -> Vec<String> {
    let base: Vec<String> = entries
        .iter()
        .map(|(_, description)| {
            let trimmed = description.trim();
            if trimmed.is_empty() {
                generic.to_string()
            } else {
                trimmed.to_string()
            }
        })
        .collect();

    base.iter()
        .zip(entries)
        .map(|(label, (id, _))| {
            if base.iter().filter(|other| *other == label).count() > 1 {
                format!("{} ({})", label, short_id(id))
            } else {
                label.clone()
            }
        })
        .collect()
}

fn pulse_source_devices(sources: &[PulseSource], default_name: Option<&str>) -> Vec<AudioDevice> {
    let entries: Vec<(&str, &str)> = sources
        .iter()
        .map(|s| (s.source_name.as_str(), s.description.as_str()))
        .collect();
    let labels = role_labels(&entries, GENERIC_INPUT_LABEL);
    sources
        .iter()
        .zip(labels)
        .map(|(source, label)| {
            AudioDevice::new(source.source_name.clone(), DeviceType::Input)
                .with_label(label)
                .with_default(Some(source.source_name.as_str()) == default_name)
        })
        .collect()
}

fn pulse_sink_devices(sinks: &[PulseSink], default_name: Option<&str>) -> Vec<AudioDevice> {
    let entries: Vec<(&str, &str)> = sinks
        .iter()
        .map(|s| (s.name.as_str(), s.description.as_str()))
        .collect();
    let labels = role_labels(&entries, GENERIC_OUTPUT_LABEL);
    sinks
        .iter()
        .zip(labels)
        .map(|(sink, label)| {
            AudioDevice::new(sink.name.clone(), DeviceType::Output)
                .with_label(label)
                .with_default(Some(sink.name.as_str()) == default_name)
        })
        .collect()
}

/// Configure Linux audio devices.
///
/// In the nominal case (PulseAudio/PipeWire server reachable) both the
/// microphone list and the system-audio list come from the server's own
/// introspection: real human-readable descriptions, no ~/.asoundrc monitor-hint
/// scanning, and no cpal/ALSA enumeration at all. This is the only way to show
/// the same labels as the desktop's native audio settings (KDE, GNOME…).
///
/// If the server is unreachable, fall back to a short whitelist of ALSA
/// endpoints (`default`, `pipewire`, `pulse`) so the list is never empty.
///
/// See audio/capture/pulse_linux.rs for the actual capture implementation.
pub fn configure_linux_audio(host: &cpal::Host) -> Result<Vec<AudioDevice>> {
    configure_linux_audio_with_status(host).map(|(devices, _)| devices)
}

/// Same as [`configure_linux_audio`], plus how complete each role's list is.
pub fn configure_linux_audio_with_status(
    host: &cpal::Host,
) -> Result<(Vec<AudioDevice>, LinuxInventoryStatus)> {
    let mut devices = Vec::new();
    let mut sources_result: Result<(), String> = Ok(());
    let mut sinks_result: Result<(), String> = Ok(());

    // ------------------------------------------------------------------
    // Microphones — nominal path: real PulseAudio/PipeWire input sources.
    // ------------------------------------------------------------------
    match list_pulse_sources() {
        Ok(sources) => {
            info!(
                "🎙️ configure_linux_audio: PulseAudio/PipeWire gave {} input source(s)",
                sources.len()
            );
            let default_source = default_pulse_source_name().unwrap_or_else(|e| {
                warn!("configure_linux_audio: default source query failed: {}", e);
                None
            });
            devices.extend(pulse_source_devices(&sources, default_source.as_deref()));
        }
        Err(e) => {
            warn!(
                "🎙️ configure_linux_audio: failed to list PulseAudio/PipeWire sources ({}); falling back to filtered ALSA endpoints",
                e
            );
            sources_result = Err(e.to_string());

            // Serialize every alsa-lib call: see ALSA_GLOBAL_LOCK doc comment above.
            let input_devices: Vec<_> = {
                let _guard = ALSA_GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                host.input_devices()?.collect()
            };

            let mut kept_any = false;
            for device in input_devices {
                if let Ok(name) = device.name() {
                    if is_useful_alsa_endpoint(&name) {
                        devices.push(AudioDevice::new(name, DeviceType::Input));
                        kept_any = true;
                    }
                }
            }

            if !kept_any {
                warn!("🎙️ configure_linux_audio: no useful ALSA endpoint found; adding 'default' so the list is never empty");
                devices.push(AudioDevice::new("default".to_string(), DeviceType::Input));
            }
        }
    }

    // ------------------------------------------------------------------
    // System Audio — unchanged from US-01: PulseAudio/PipeWire sinks.
    // ------------------------------------------------------------------
    match list_pulse_sinks() {
        Ok(sinks) => {
            info!(
                "🎙️ configure_linux_audio: list_pulse_sinks returned {} sink(s)",
                sinks.len()
            );
            let default_sink = default_pulse_sink_name().unwrap_or_else(|e| {
                warn!("configure_linux_audio: default sink query failed: {}", e);
                None
            });
            devices.extend(pulse_sink_devices(&sinks, default_sink.as_deref()));
        }
        Err(e) => {
            warn!("Failed to list PulseAudio/PipeWire sinks for System Audio: {}", e);
            sinks_result = Err(e.to_string());
        }
    }

    remember_labels(&devices);
    Ok((devices, inventory_status(sources_result, sinks_result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(name: &str, description: &str) -> PulseSource {
        PulseSource {
            description: description.to_string(),
            source_name: name.to_string(),
        }
    }

    fn sink(name: &str, description: &str) -> PulseSink {
        PulseSink {
            name: name.to_string(),
            description: description.to_string(),
            monitor_source_name: format!("{}.monitor", name),
        }
    }

    #[test]
    fn inventory_status_maps_pulse_results_to_role_statuses() {
        let both_ok = inventory_status(Ok(()), Ok(()));
        assert_eq!(both_ok.microphone.status, InventoryStatus::Complete);
        assert_eq!(both_ok.system.status, InventoryStatus::Complete);
        assert!(both_ok.microphone.error.is_none());

        let sources_err = inventory_status(Err("no server".into()), Ok(()));
        assert_eq!(sources_err.microphone.status, InventoryStatus::Degraded);
        assert_eq!(sources_err.microphone.error.as_deref(), Some("no server"));
        assert_eq!(sources_err.system.status, InventoryStatus::Complete);

        let sinks_err = inventory_status(Ok(()), Err("boom".into()));
        assert_eq!(sinks_err.microphone.status, InventoryStatus::Complete);
        assert_eq!(sinks_err.system.status, InventoryStatus::Failed);
        assert_eq!(sinks_err.system.error.as_deref(), Some("boom"));
    }

    #[test]
    fn cached_label_falls_back_to_id_then_serves_remembered_label() {
        assert_eq!(cached_label("cache_test_id"), "cache_test_id");
        remember_labels(&[AudioDevice::new("cache_test_id".into(), DeviceType::Input)
            .with_label("Readable".into())]);
        assert_eq!(cached_label("cache_test_id"), "Readable");
    }

    #[test]
    fn unique_labels_are_left_alone() {
        let devices = pulse_source_devices(
            &[source("alsa_input.a", "Mic A"), source("alsa_input.b", "Mic B")],
            None,
        );
        assert_eq!(devices[0].label.as_deref(), Some("Mic A"));
        assert_eq!(devices[1].label.as_deref(), Some("Mic B"));
    }

    #[test]
    fn duplicate_labels_in_a_role_get_short_id_suffix_but_keep_exact_ids() {
        let devices = pulse_source_devices(
            &[
                source("alsa_input.usb-1.analog", "USB Mic"),
                source("alsa_input.usb-2.analog", "USB Mic"),
            ],
            None,
        );
        assert_eq!(devices[0].name, "alsa_input.usb-1.analog");
        assert_eq!(devices[0].label.as_deref(), Some("USB Mic (usb-1.analog)"));
        assert_eq!(devices[1].label.as_deref(), Some("USB Mic (usb-2.analog)"));
    }

    #[test]
    fn same_description_across_roles_is_not_disambiguated() {
        let inputs = pulse_source_devices(&[source("alsa_input.h", "Headset")], None);
        let outputs = pulse_sink_devices(&[sink("alsa_output.h", "Headset")], None);
        assert_eq!(inputs[0].label.as_deref(), Some("Headset"));
        assert_eq!(outputs[0].label.as_deref(), Some("Headset"));
        assert_ne!(inputs[0], outputs[0]);
    }

    #[test]
    fn empty_description_gets_generic_role_label_not_raw_id() {
        let inputs = pulse_source_devices(&[source("alsa_input.x", "  ")], None);
        let outputs = pulse_sink_devices(&[sink("alsa_output.x", "")], None);
        assert_eq!(inputs[0].label.as_deref(), Some("Microphone"));
        assert_eq!(outputs[0].label.as_deref(), Some("Sortie audio"));
    }

    #[test]
    fn default_flag_marks_exact_id_only() {
        let devices = pulse_sink_devices(
            &[sink("sink_a", "Speakers"), sink("sink_b", "Speakers")],
            Some("sink_b"),
        );
        assert!(!devices[0].is_default);
        assert!(devices[1].is_default);
    }

    #[test]
    fn sink_device_name_has_no_display_suffix() {
        let devices = pulse_sink_devices(&[sink("sink_a", "Speakers")], None);
        assert_eq!(devices[0].name, "sink_a");
        assert_eq!(devices[0].device_type, DeviceType::Output);
    }

    #[test]
    fn test_is_useful_alsa_endpoint_keeps_whitelist() {
        for name in ["default", "pipewire", "pulse", "DEFAULT", "PipeWire", "PULSE"] {
            assert!(
                is_useful_alsa_endpoint(name),
                "'{}' should be kept as a useful ALSA endpoint",
                name
            );
        }
    }

    #[test]
    fn test_is_useful_alsa_endpoint_rejects_noise() {
        let rejected = [
            "null",
            "lavrate",
            "samplerate",
            "speexrate",
            "jack",
            "oss",
            "speex",
            "upmix",
            "vdownmix",
            "usbstream:CARD=Generic",
            "sysdefault:CARD=Generic_1",
            "sysdefault:CARD=acppdmmach",
            "sysdefault:CARD=J380",
            "hw:0,0",
            "plughw:0,0",
            "front:CARD=Generic,DEV=0",
            "surround21:CARD=Generic",
            "iec958:CARD=Generic",
            "hdmi:CARD=HDMI,DEV=0",
            "dmix:CARD=Generic",
            "dsnoop:CARD=Generic",
            "Ryzen_HD_Audio_Controller_Speaker_monitor",
        ];

        for name in rejected {
            assert!(
                !is_useful_alsa_endpoint(name),
                "'{}' should be rejected as ALSA noise",
                name
            );
        }
    }
}