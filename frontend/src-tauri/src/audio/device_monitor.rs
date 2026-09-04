// Audio device monitoring for disconnect/reconnect detection
use anyhow::Result;
use log::{debug, error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::devices::{list_audio_devices, AudioDevice, DeviceType as AudioDeviceType};

/// Device monitoring events
#[derive(Debug, Clone)]
pub enum DeviceEvent {
    /// A device that was in use has disconnected
    DeviceDisconnected {
        device_name: String,
        device_type: DeviceMonitorType,
    },
    /// A previously disconnected device has reconnected
    DeviceReconnected {
        device_name: String,
        device_type: DeviceMonitorType,
    },
    /// Device list has changed (new device added or removed)
    DeviceListChanged,
}

/// Type of device being monitored
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceMonitorType {
    Microphone,
    SystemAudio,
}

/// Monitor state for a single device
#[derive(Debug, Clone)]
struct MonitoredDevice {
    name: String,
    device_type: DeviceMonitorType,
    consecutive_missing: u32,
    is_disconnected: bool,
    is_bluetooth: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DevicePresenceTransition {
    Disconnected,
    Reconnected,
}

impl MonitoredDevice {
    fn new(name: String, device_type: DeviceMonitorType) -> Self {
        // Heuristic: check if device name contains bluetooth-related keywords
        let is_bluetooth = name.to_lowercase().contains("airpods")
            || name.to_lowercase().contains("bluetooth")
            || name.to_lowercase().contains("wireless");

        Self {
            name,
            device_type,
            consecutive_missing: 0,
            is_disconnected: false,
            is_bluetooth,
        }
    }

    /// Get appropriate disconnect threshold based on device type
    fn disconnect_threshold(&self) -> u32 {
        // Bluetooth devices get more grace period (they can briefly disconnect)
        if self.is_bluetooth {
            3 // 3 polling cycles (6-15 seconds)
        } else {
            2 // 2 polling cycles (4-10 seconds)
        }
    }

    /// Get appropriate reconnect check interval
    #[allow(dead_code)]
    fn reconnect_interval(&self) -> Duration {
        if self.is_bluetooth {
            Duration::from_secs(5) // Check every 5s for Bluetooth
        } else {
            Duration::from_secs(3) // Check every 3s for wired devices
        }
    }

    fn observe_presence(&mut self, is_present: bool) -> Option<DevicePresenceTransition> {
        if is_present {
            self.consecutive_missing = 0;
            if self.is_disconnected {
                self.is_disconnected = false;
                Some(DevicePresenceTransition::Reconnected)
            } else {
                None
            }
        } else {
            self.consecutive_missing = self.consecutive_missing.saturating_add(1);
            if !self.is_disconnected && self.consecutive_missing >= self.disconnect_threshold() {
                self.is_disconnected = true;
                Some(DevicePresenceTransition::Disconnected)
            } else {
                None
            }
        }
    }
}

fn monitored_device_is_present(monitored: &MonitoredDevice, devices: &[AudioDevice]) -> bool {
    let expected_type = match &monitored.device_type {
        DeviceMonitorType::Microphone => AudioDeviceType::Input,
        DeviceMonitorType::SystemAudio => AudioDeviceType::Output,
    };
    devices
        .iter()
        .any(|device| device.name == monitored.name && device.device_type == expected_type)
}

fn next_check_interval(monitored_devices: &[MonitoredDevice]) -> Duration {
    if monitored_devices
        .iter()
        .any(|device| device.consecutive_missing > 0)
    {
        Duration::from_secs(2)
    } else {
        Duration::from_secs(5)
    }
}

/// Audio device monitor that detects disconnects and reconnects
pub struct AudioDeviceMonitor {
    monitor_handle: Option<JoinHandle<()>>,
    event_sender: mpsc::UnboundedSender<DeviceEvent>,
    stop_signal: Arc<tokio::sync::Notify>,
}

impl AudioDeviceMonitor {
    /// Create a new device monitor
    pub fn new() -> (Self, mpsc::UnboundedReceiver<DeviceEvent>) {
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let stop_signal = Arc::new(tokio::sync::Notify::new());

        (
            Self {
                monitor_handle: None,
                event_sender,
                stop_signal,
            },
            event_receiver,
        )
    }

    /// Start monitoring specified devices
    pub fn start_monitoring(
        &mut self,
        microphone: Option<Arc<AudioDevice>>,
        system_audio: Option<Arc<AudioDevice>>,
    ) -> Result<()> {
        if self.monitor_handle.is_some() {
            warn!("Device monitor already running");
            return Ok(());
        }

        let mut monitored_devices = Vec::new();

        if let Some(mic) = microphone {
            monitored_devices.push(MonitoredDevice::new(
                mic.name.clone(),
                DeviceMonitorType::Microphone,
            ));
            info!(
                "🔍 Monitoring microphone: '{}' (Bluetooth: {})",
                mic.name,
                monitored_devices.last().unwrap().is_bluetooth
            );
        }

        if let Some(sys) = system_audio {
            monitored_devices.push(MonitoredDevice::new(
                sys.name.clone(),
                DeviceMonitorType::SystemAudio,
            ));
            info!(
                "🔍 Monitoring system audio: '{}' (Bluetooth: {})",
                sys.name,
                monitored_devices.last().unwrap().is_bluetooth
            );
        }

        if monitored_devices.is_empty() {
            return Err(anyhow::anyhow!("No devices to monitor"));
        }

        let event_sender = self.event_sender.clone();
        let stop_signal = self.stop_signal.clone();

        let handle = tokio::spawn(async move {
            Self::monitor_loop(monitored_devices, event_sender, stop_signal).await;
        });

        self.monitor_handle = Some(handle);
        info!("✅ Device monitor started");
        Ok(())
    }

    /// Stop monitoring
    pub async fn stop_monitoring(&mut self) {
        info!("Stopping device monitor");
        self.stop_signal.notify_one();

        if let Some(handle) = self.monitor_handle.take() {
            let _ = handle.await;
        }

        info!("Device monitor stopped");
    }

    /// Main monitoring loop
    async fn monitor_loop(
        mut monitored_devices: Vec<MonitoredDevice>,
        event_sender: mpsc::UnboundedSender<DeviceEvent>,
        stop_signal: Arc<tokio::sync::Notify>,
    ) {
        let mut last_device_list = Vec::new();
        let mut check_interval = Duration::from_secs(2); // Poll quickly until the first snapshot

        loop {
            // Check for stop signal with timeout
            tokio::select! {
                _ = stop_signal.notified() => {
                    info!("Device monitor received stop signal");
                    break;
                }
                _ = tokio::time::sleep(check_interval) => {
                    // Continue with monitoring check
                }
            }

            // Enumeration runs on a blocking worker. Listening for stop in
            // parallel lets shutdown detach a stuck driver enumeration instead
            // of holding the recording supervisor indefinitely.
            let current_devices = tokio::select! {
                _ = stop_signal.notified() => {
                    info!("Device monitor stopped during device enumeration");
                    break;
                }
                result = list_audio_devices() => match result {
                    Ok(devices) => devices,
                    Err(e) => {
                        error!("Failed to list audio devices: {}", e);
                        continue;
                    }
                }
            };

            // Check if device list changed
            if current_devices.len() != last_device_list.len() {
                debug!(
                    "Device list changed: {} -> {} devices",
                    last_device_list.len(),
                    current_devices.len()
                );
                let _ = event_sender.send(DeviceEvent::DeviceListChanged);
            }
            last_device_list = current_devices.clone();

            // Check each monitored device
            for monitored in &mut monitored_devices {
                let device_found = monitored_device_is_present(monitored, &current_devices);
                let previous_missing = monitored.consecutive_missing;

                match monitored.observe_presence(device_found) {
                    Some(DevicePresenceTransition::Reconnected) => {
                        info!(
                            "✅ Device '{}' reconnected after {} missing checks",
                            monitored.name, previous_missing
                        );

                        let _ = event_sender.send(DeviceEvent::DeviceReconnected {
                            device_name: monitored.name.clone(),
                            device_type: monitored.device_type.clone(),
                        });
                    }
                    Some(DevicePresenceTransition::Disconnected) => {
                        warn!(
                            "❌ Device '{}' ({:?}) disconnected!",
                            monitored.name, monitored.device_type
                        );

                        let _ = event_sender.send(DeviceEvent::DeviceDisconnected {
                            device_name: monitored.name.clone(),
                            device_type: monitored.device_type.clone(),
                        });
                    }
                    None if !device_found => {
                        debug!(
                            "⚠️ Device '{}' missing for {} checks (threshold: {})",
                            monitored.name,
                            monitored.consecutive_missing,
                            monitored.disconnect_threshold()
                        );
                    }
                    None if previous_missing > 0 => {
                        debug!(
                            "Device '{}' recovered before the disconnect threshold; ignoring transient dropout",
                            monitored.name
                        );
                    }
                    None => {}
                }
            }

            // Adjust check interval based on device states
            // If any device is missing, check more frequently
            let next_interval = next_check_interval(&monitored_devices);

            if next_interval != check_interval {
                debug!("Adjusting monitor interval to {:?}", next_interval);
                check_interval = next_interval;
            }
        }
    }
}

impl Default for AudioDeviceMonitor {
    fn default() -> Self {
        Self::new().0
    }
}

impl Drop for AudioDeviceMonitor {
    fn drop(&mut self) {
        // Signal stop
        self.stop_signal.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_name_with_wrong_direction_is_not_the_monitored_endpoint() {
        let microphone = MonitoredDevice::new(
            "Shared endpoint name".to_string(),
            DeviceMonitorType::Microphone,
        );
        let output_only = vec![AudioDevice::new(
            "Shared endpoint name".to_string(),
            AudioDeviceType::Output,
        )];
        let matching_input = vec![AudioDevice::new(
            "Shared endpoint name".to_string(),
            AudioDeviceType::Input,
        )];

        assert!(!monitored_device_is_present(&microphone, &output_only));
        assert!(monitored_device_is_present(&microphone, &matching_input));
    }

    #[test]
    fn test_bluetooth_detection() {
        let airpods = MonitoredDevice::new(
            "John's AirPods Pro".to_string(),
            DeviceMonitorType::Microphone,
        );
        assert!(airpods.is_bluetooth);
        assert_eq!(airpods.disconnect_threshold(), 3);

        let builtin = MonitoredDevice::new(
            "Built-in Microphone".to_string(),
            DeviceMonitorType::Microphone,
        );
        assert!(!builtin.is_bluetooth);
        assert_eq!(builtin.disconnect_threshold(), 2);
    }

    #[test]
    fn test_monitor_interval_tracks_device_health() {
        let healthy = MonitoredDevice::new(
            "Built-in Microphone".to_string(),
            DeviceMonitorType::Microphone,
        );
        assert_eq!(
            next_check_interval(&[healthy.clone()]),
            Duration::from_secs(5)
        );

        let mut missing = healthy;
        missing.consecutive_missing = 1;
        assert_eq!(next_check_interval(&[missing]), Duration::from_secs(2));
    }

    #[test]
    fn transient_dropout_does_not_emit_reconnected() {
        let mut device = MonitoredDevice::new(
            "Built-in Microphone".to_string(),
            DeviceMonitorType::Microphone,
        );

        assert_eq!(device.observe_presence(false), None);
        assert!(!device.is_disconnected);
        assert_eq!(device.observe_presence(true), None);
        assert_eq!(device.consecutive_missing, 0);
        assert!(!device.is_disconnected);
    }

    #[test]
    fn reconnect_requires_a_prior_disconnect_transition() {
        let mut device = MonitoredDevice::new(
            "Built-in Microphone".to_string(),
            DeviceMonitorType::Microphone,
        );

        assert_eq!(device.observe_presence(false), None);
        assert_eq!(
            device.observe_presence(false),
            Some(DevicePresenceTransition::Disconnected)
        );
        assert!(device.is_disconnected);

        // Further missing polls must not repeat the disconnect event.
        assert_eq!(device.observe_presence(false), None);
        assert_eq!(
            device.observe_presence(true),
            Some(DevicePresenceTransition::Reconnected)
        );
        assert!(!device.is_disconnected);

        // Presence remains stable and must not repeat the reconnect event.
        assert_eq!(device.observe_presence(true), None);
    }

    #[tokio::test]
    async fn test_monitor_creation() {
        let (mut monitor, _receiver) = AudioDeviceMonitor::new();
        assert!(monitor.monitor_handle.is_none());

        // Stop should be safe even if not started
        monitor.stop_monitoring().await;
    }
}
