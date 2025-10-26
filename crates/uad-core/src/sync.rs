use crate::{
    adb::{ACommand as AdbCommand, PM_CLEAR_PACK},
    uad_lists::PackageState,
};
use log::{error, info};
use retry::{OperationResult, delay::Fixed, retry};
use serde::{Deserialize, Serialize};

/// An Android device, typically a phone
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phone {
    /// Non-market name
    pub model: String, // could be `Copy`
    /// Android API level version
    pub android_sdk: u8,
    /// In theory, `len < u16::MAX` _should_ always be `true`.
    /// In practice, `len <= u8::MAX`.
    pub user_list: Vec<User>,
    /// Unique serial identifier
    pub adb_id: String, // could be `Copy`
}

impl Default for Phone {
    fn default() -> Self {
        Self {
            model: "fetching devices...".to_string(),
            android_sdk: 0,
            user_list: vec![],
            adb_id: String::default(),
        }
    }
}

impl std::fmt::Display for Phone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.model)
    }
}

/// `UserInfo` but relevant to UAD
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct User {
    pub id: u16,
    pub index: usize,
    pub protected: bool,
}

impl std::fmt::Display for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "user {}", self.id)
    }
}

/// An enum to contain different variants for errors yielded by ADB.
#[derive(Debug, Clone)]
pub enum AdbError {
    Generic(String),
}

/// Run an arbitrary shell action via the typed ADB wrapper.
/// This replaces the deprecated `adb_shell_command`.
pub async fn run_adb_shell_action<S: AsRef<str>>(
    device_serial: S,
    action: String,
) -> Result<String, AdbError> {
    let serial = device_serial.as_ref();

    match AdbCommand::new().shell(serial).raw(&action) {
        Ok(o) => {
            if ["Error", "Failure"].iter().any(|&e| o.contains(e)) {
                let friendly_msg = make_friendly_error_message(&o, &action);
                return Err(AdbError::Generic(friendly_msg));
            }
            info!("{action} -> {o}");
            Ok(o)
        }
        Err(err) => {
            if !err.contains("[not installed for") {
                let friendly_msg = make_friendly_error_message(&err, &action);
                return Err(AdbError::Generic(friendly_msg));
            }
            Err(AdbError::Generic(err))
        }
    }
}

/// Convert common OEM-specific ADB error messages into user-friendly explanations.
fn make_friendly_error_message(error_output: &str, action: &str) -> String {
    // Common Samsung errors
    if error_output.contains("DELETE_FAILED_USER_RESTRICTED") {
        return format!(
            "Cannot uninstall: This package is restricted by the device manufacturer (Samsung Knox or similar).\n\
            Error: {}\n\
            Tip: Try disabling the package instead, or check device settings for Knox/security restrictions.",
            error_output
        );
    }

    if error_output.contains("NOT_INSTALLED_FOR_USER") {
        return format!(
            "Package is not installed for the current user.\n\
            Error: {}\n\
            Tip: The package may be installed for a different user profile or work profile.",
            error_output
        );
    }

    // Empty package name error
    if error_output.contains("Shell cannot change component state for null") {
        return format!(
            "Invalid package: Empty package name detected.\n\
            Error: {}\n\
            Tip: Please refresh the package list and try again.",
            error_output
        );
    }

    // Generic permission errors
    if error_output.contains("Permission denied")
        || error_output.contains("INSTALL_FAILED_PERMISSION_MODEL_DOWNGRADE")
    {
        return format!(
            "Permission denied: Insufficient privileges to perform this action.\n\
            Error: {}\n\
            Tip: This may require root access or the package is protected by the system.",
            error_output
        );
    }

    // Work profile / managed device errors
    if error_output.contains("DELETE_FAILED_DEVICE_POLICY_MANAGER") {
        return format!(
            "Cannot modify: Package is managed by device policy (MDM/EMM).\n\
            Error: {}\n\
            Tip: Contact your IT administrator if this is a work device.",
            error_output
        );
    }

    // Generic failure with context
    format!("{} -> {}", action, error_output)
}

/// If `None`, returns an empty String, not " --user 0"
pub fn user_flag(user_id: Option<User>) -> String {
    user_id
        .map(|user| format!(" --user {}", user.id))
        .unwrap_or_default()
}

// Minimum information for processing adb commands
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
pub struct CorePackage {
    pub name: String,
    pub state: PackageState,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub removal: crate::uad_lists::Removal,
}

pub fn apply_pkg_state_commands(
    package: &CorePackage,
    wanted_state: PackageState,
    selected_user: User,
    phone: &Phone,
) -> Vec<String> {
    // https://github.com/Universal-Debloater-Alliance/universal-android-debloater/wiki/ADB-reference
    // ALWAYS PUT THE COMMAND THAT CHANGES THE PACKAGE STATE FIRST!
    let commands = match wanted_state {
        PackageState::Enabled => match package.state {
            PackageState::Disabled => vec!["pm enable"],
            PackageState::Uninstalled => match phone.android_sdk {
                i if i >= 23 => vec!["cmd package install-existing"],
                21 | 22 => vec!["pm unhide"],
                19 | 20 => vec!["pm unblock", PM_CLEAR_PACK],
                _ => unreachable!("already prevented by the GUI"),
            },
            _ => vec![],
        },
        PackageState::Disabled => match package.state {
            PackageState::Uninstalled | PackageState::Enabled => match phone.android_sdk {
                sdk if sdk >= 23 => vec!["pm disable-user", "am force-stop", PM_CLEAR_PACK],
                _ => vec![],
            },
            _ => vec![],
        },
        PackageState::Uninstalled => match package.state {
            PackageState::Enabled | PackageState::Disabled => match phone.android_sdk {
                sdk if sdk >= 23 => vec!["pm uninstall"], // > Android Marshmallow (6.0)
                21 | 22 => vec!["pm hide", PM_CLEAR_PACK], // Android Lollipop (5.x)
                _ => vec!["pm block", PM_CLEAR_PACK], // Disable mode is unavailable on older devices because the specific ADB commands need root
            },
            _ => vec![],
        },
        PackageState::All => vec![],
    }; // this should be a `tinyvec`, as `len <= 4`

    let user = supports_multi_user(phone).then_some(selected_user);
    request_builder(&commands, &package.name, user)
}

/// Build a command request to be sent via ADB to a device.
/// `commands` accepts one or more ADB shell commands
/// which act on a common `package` and `user`.
pub fn request_builder(commands: &[&str], package: &str, user: Option<User>) -> Vec<String> {
    let maybe_user_flag = user_flag(user);
    commands
        .iter()
        .map(|c| format!("{c}{maybe_user_flag} {package}"))
        .collect()
}

/// Get the model by querying the `ro.product.model` property.
///
/// If `serial` is empty, it lets ADB choose the default device.
pub fn get_device_model(serial: &str) -> String {
    AdbCommand::new()
        .shell(serial)
        .getprop("ro.product.model")
        .unwrap_or_else(|err| {
            eprintln!("ERROR: {err}");
            error!("{err}");
            if err.contains("adb: no devices/emulators found") {
                "no devices/emulators found".to_string()
            } else {
                err
            }
        })
}

/// Get the brand by querying the `ro.product.brand` property.
///
/// If `serial` is empty, it lets ADB choose the default device.
pub fn get_device_brand(serial: &str) -> String {
    AdbCommand::new()
        .shell(serial)
        .getprop("ro.product.brand")
        // `trim` is just-in-case
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Get Android SDK version by querying the
// `ro.build.version.sdk` property or defaulting to 0.
///
/// If `device_serial` is empty, it lets ADB choose the default device.
pub fn get_android_sdk(device_serial: &str) -> u8 {
    AdbCommand::new()
        .shell(device_serial)
        .getprop("ro.build.version.sdk")
        .map_or(0, |sdk| {
            sdk.parse().expect("SDK version numeral must be valid")
        })
}

/// Detect cross-user behavior and return appropriate notification message.
/// This handles all three cases of unexpected cross-user behavior:
/// - Case A: Uninstall → Restore (package appears on other users)
/// - Case B: Uninstall → Uninstall (package disappears from other users)  
/// - Case C: Restore → Restore (package appears on other users)
pub fn detect_cross_user_behavior(
    package_name: &str,
    device_serial: &str,
    target_user_id: u16,
    wanted_state: PackageState,
    actual_state: PackageState,
    phone: &Phone,
) -> Option<String> {
    // Only check if operation was successful on target user
    if actual_state != wanted_state {
        return None;
    }

    // Only check if we have multiple users
    if phone.user_list.len() < 2 {
        return None;
    }

    let cross_user_states =
        check_cross_user_package_existence(package_name, device_serial, target_user_id, phone);

    match wanted_state {
        PackageState::Uninstalled => {
            if !cross_user_states.is_empty() {
                // Case A: Uninstall → Restore
                let user_list = cross_user_states
                    .iter()
                    .map(|(uid, state)| format!("user {} ({:?})", uid, state))
                    .collect::<Vec<_>>()
                    .join(", ");

                Some(format!(
                    "Detected cross-user restoration: package exists on {} after uninstalling from user {}",
                    user_list, target_user_id
                ))
            } else {
                // Case B: Uninstall → Uninstall (check if all users lost package)
                let affected_users: Vec<_> = phone
                    .user_list
                    .iter()
                    .filter(|u| !u.protected && u.id != target_user_id)
                    .filter(|u| {
                        verify_package_state(package_name, device_serial, Some(u.id))
                            == PackageState::Uninstalled
                    })
                    .map(|u| u.id)
                    .collect();

                if !affected_users.is_empty() {
                    let user_list = affected_users
                        .iter()
                        .map(|uid| format!("user {}", uid))
                        .collect::<Vec<_>>()
                        .join(", ");

                    Some(format!(
                        "Detected cross-user uninstall: package was also uninstalled from {} after uninstalling from user {}",
                        user_list, target_user_id
                    ))
                } else {
                    None
                }
            }
        }
        PackageState::Enabled | PackageState::Disabled => {
            // Case C: Restore → Restore
            if !cross_user_states.is_empty() {
                let user_list = cross_user_states
                    .iter()
                    .map(|(uid, state)| format!("user {} ({:?})", uid, state))
                    .collect::<Vec<_>>()
                    .join(", ");

                Some(format!(
                    "Detected cross-user restoration: package exists on {} after {} from user {}",
                    user_list,
                    if wanted_state == PackageState::Enabled {
                        "enabling"
                    } else {
                        "disabling"
                    },
                    target_user_id
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Minimum inclusive Android SDK version
/// that supports multi-user mode.
/// Lollipop 5.0
pub const MULTI_USER_SDK: u8 = 21;

/// Check if it might support multi-user mode,
/// by simply comparing SDK version.
/// `true` isn't reliable, you can only trust `false`.
///
/// See:
/// - <https://source.android.com/docs/devices/admin/multi-user#applying_the_overlay>
/// - <https://developer.android.com/reference/android/os/UserManager#supportsMultipleUsers()>
#[must_use]
pub const fn supports_multi_user(dev: &Phone) -> bool {
    dev.android_sdk >= MULTI_USER_SDK
}

/// Check if a `user_id` is protected on a device by trying
/// to list associated packages.
///
/// If `device_serial` is empty, it lets ADB choose the default device.
pub fn is_protected_user<S: AsRef<str>>(user_id: u16, device_serial: S) -> bool {
    AdbCommand::new()
        .shell(device_serial)
        .pm()
        .list_packages_sys(None, Some(user_id))
        .is_err()
}

pub fn list_users_idx_prot(device_serial: &str) -> Vec<User> {
    AdbCommand::new()
        .shell(device_serial)
        .pm()
        .list_users()
        .map(|out| {
            out.into_iter()
                .enumerate()
                .map(|(i, user)| {
                    let id = user.get_id();
                    User {
                        id,
                        index: i,
                        protected: is_protected_user(id, device_serial),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// This matches serials (`getprop ro.serialno`)
/// that are authorized by the user.
pub async fn get_devices_list() -> Vec<Phone> {
    retry(
        Fixed::from_millis(500).take(if cfg!(debug_assertions) { 3 } else { 10 }),
        || match AdbCommand::new().devices() {
            Ok(devices) => {
                let mut device_list: Vec<Phone> = vec![];
                if devices.iter().all(|(_, stat)| stat != "device") {
                    return OperationResult::Retry(vec![]);
                }
                for device in devices {
                    let serial = &device.0;
                    device_list.push(Phone {
                        model: format!("{} {}", get_device_brand(serial), get_device_model(serial)),
                        android_sdk: get_android_sdk(serial),
                        user_list: list_users_idx_prot(serial),
                        adb_id: serial.clone(),
                    });
                }
                OperationResult::Ok(device_list)
            }
            Err(err) => {
                error!("get_devices_list() -> {err}");
                let test: Vec<Phone> = vec![];
                OperationResult::Retry(test)
            }
        },
    )
    .unwrap_or_default()
}

pub async fn initial_load() -> bool {
    match AdbCommand::new().devices() {
        Ok(_devices) => true,
        Err(_err) => false,
    }
}

/// Get the current state of a package on a device
pub fn get_package_state(
    device_serial: &str,
    package_name: &str,
    user_id: Option<u16>,
) -> Result<PackageState, String> {
    use crate::adb::PmListPacksFlag;

    // Check if package is enabled
    if let Ok(enabled_packages) = AdbCommand::new()
        .shell(device_serial)
        .pm()
        .list_packages_sys(Some(PmListPacksFlag::OnlyEnabled), user_id)
    {
        if enabled_packages.contains(&package_name.to_string()) {
            return Ok(PackageState::Enabled);
        }
    }

    // Check if package is disabled
    if let Ok(disabled_packages) = AdbCommand::new()
        .shell(device_serial)
        .pm()
        .list_packages_sys(Some(PmListPacksFlag::OnlyDisabled), user_id)
    {
        if disabled_packages.contains(&package_name.to_string()) {
            return Ok(PackageState::Disabled);
        }
    }

    // If not in either list, assume it's uninstalled (or doesn't exist)
    Ok(PackageState::Uninstalled)
}

/// Verify the current state of a package on a device
pub fn verify_package_state(
    device_serial: &str,
    package_name: &str,
    user_id: Option<u16>,
) -> PackageState {
    get_package_state(device_serial, package_name, user_id).unwrap_or(PackageState::Uninstalled)
}

/// Check if a package exists on any other users besides the target user.
/// This helps detect OEM-specific cross-user restoration behavior.
pub fn check_cross_user_package_existence(
    package_name: &str,
    device_serial: &str,
    target_user_id: u16,
    phone: &Phone,
) -> Vec<(u16, PackageState)> {
    let mut other_user_states = Vec::new();

    for user in phone.user_list.iter() {
        if user.id != target_user_id && !user.protected {
            let state = verify_package_state(package_name, device_serial, Some(user.id));
            if state != PackageState::Uninstalled {
                other_user_states.push((user.id, state));
            }
        }
    }

    other_user_states
}

/// Attempt fallback action when package state verification fails
pub fn attempt_fallback(
    package: &CorePackage,
    wanted_state: PackageState,
    actual_state: PackageState,
    user: User,
    phone: &Phone,
) -> Result<String, String> {
    match (wanted_state, actual_state) {
        // Case 1: Tried to uninstall but package was reinstalled -> disable it
        (PackageState::Uninstalled, PackageState::Enabled) => {
            let core_package = CorePackage {
                name: package.name.clone(),
                state: PackageState::Enabled,
                description: package.description.clone(),
                removal: package.removal,
            };
            let commands =
                apply_pkg_state_commands(&core_package, PackageState::Disabled, user, phone);

            if !commands.is_empty() {
                // Execute the disable command
                let action = commands[0].clone();
                match AdbCommand::new().shell(&phone.adb_id).raw(&action) {
                    Ok(_) => Ok("disabled package instead of uninstalling".to_string()),
                    Err(err) => Err(format!("Failed to disable package: {err}")),
                }
            } else {
                Err("No disable command available for this Android version".to_string())
            }
        }

        // Case 2: Tried to disable but package re-enabled itself -> try uninstall
        (PackageState::Disabled, PackageState::Enabled) => {
            let core_package = CorePackage {
                name: package.name.clone(),
                state: PackageState::Enabled,
                description: package.description.clone(),
                removal: package.removal,
            };
            let commands =
                apply_pkg_state_commands(&core_package, PackageState::Uninstalled, user, phone);

            if !commands.is_empty() {
                // Execute the uninstall command
                let action = commands[0].clone();
                match AdbCommand::new().shell(&phone.adb_id).raw(&action) {
                    Ok(_) => Ok("uninstalled package instead of disabling".to_string()),
                    Err(err) => Err(format!("Failed to uninstall package: {err}")),
                }
            } else {
                Err("No uninstall command available for this Android version".to_string())
            }
        }

        // Case 3: Tried to enable but package was disabled -> try uninstall then reinstall
        (PackageState::Enabled, PackageState::Disabled) => {
            // First try to uninstall
            let core_package = CorePackage {
                name: package.name.clone(),
                state: PackageState::Disabled,
                description: package.description.clone(),
                removal: package.removal,
            };
            let uninstall_commands =
                apply_pkg_state_commands(&core_package, PackageState::Uninstalled, user, phone);

            if !uninstall_commands.is_empty() {
                let uninstall_action = uninstall_commands[0].clone();
                match AdbCommand::new()
                    .shell(&phone.adb_id)
                    .raw(&uninstall_action)
                {
                    Ok(_) => {
                        // Now try to reinstall/enable
                        let core_package_uninstalled = CorePackage {
                            name: package.name.clone(),
                            state: PackageState::Uninstalled,
                            description: package.description.clone(),
                            removal: package.removal,
                        };
                        let enable_commands = apply_pkg_state_commands(
                            &core_package_uninstalled,
                            PackageState::Enabled,
                            user,
                            phone,
                        );

                        if !enable_commands.is_empty() {
                            let enable_action = enable_commands[0].clone();
                            match AdbCommand::new().shell(&phone.adb_id).raw(&enable_action) {
                                Ok(_) => {
                                    Ok("uninstalled and reinstalled package to enable it"
                                        .to_string())
                                }
                                Err(err) => Err(format!("Failed to reinstall package: {err}")),
                            }
                        } else {
                            Ok("uninstalled package but couldn't reinstall".to_string())
                        }
                    }
                    Err(err) => Err(format!("Failed to uninstall package for reinstall: {err}")),
                }
            } else {
                Err("No uninstall command available for reinstall attempt".to_string())
            }
        }

        // Other cases - no fallback available
        _ => Err(format!(
            "No fallback available for wanted state {wanted_state:?} and actual state {actual_state:?}"
        )),
    }
}
