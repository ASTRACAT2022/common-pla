use base64::Engine;
use reqwest::redirect::{Attempt, Policy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::HashMap,
    fs,
    fs::OpenOptions,
    io::Write,
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
    thread,
    time::Duration,
};
use tauri::{AppHandle, Manager, State};
use url::Url;

const MAX_SUBSCRIPTION_BYTES: usize = 2 * 1024 * 1024;

#[derive(Default)]
struct AppState {
    profiles: Mutex<Vec<StoredProfile>>,
    active: Mutex<Option<ActiveConnection>>,
    routing: Mutex<Option<RoutingSettings>>,
}

#[derive(Clone)]
struct StoredProfile {
    summary: ProfileSummary,
    secret: String,
}

struct ActiveConnection {
    profile_id: String,
    name: String,
    protocol: String,
    config_path: PathBuf,
    process: Option<Child>,
    #[cfg(not(target_os = "linux"))]
    external_pid: Option<u32>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileSummary {
    id: String,
    name: String,
    protocol: String,
    endpoint: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    platform: String,
    xray_detected: bool,
    awg_detected: bool,
    note: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionStatus {
    connected: bool,
    profile_id: Option<String>,
    name: Option<String>,
    protocol: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoutingSettings {
    name: String,
    global_proxy: bool,
    remote_dns: String,
    domestic_dns: String,
    remote_dns_type: String,
    domestic_dns_type: String,
    remote_dns_domain: String,
    domestic_dns_domain: String,
    dns_hosts: HashMap<String, String>,
    route_order: String,
    direct_sites: Vec<String>,
    direct_ip: Vec<String>,
    proxy_sites: Vec<String>,
    proxy_ip: Vec<String>,
    block_sites: Vec<String>,
    block_ip: Vec<String>,
    domain_strategy: String,
    geoip_url: String,
    geosite_url: String,
    asset_version: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoutingSummary {
    name: String,
    global_proxy: bool,
    direct_count: usize,
    proxy_count: usize,
    block_count: usize,
}

#[tauri::command]
async fn import_subscription(
    app: AppHandle,
    url: String,
    state: State<'_, AppState>,
) -> Result<Vec<ProfileSummary>, String> {
    let subscription_url = validate_subscription_url(&url)?;
    let original_host = subscription_url
        .host_str()
        .ok_or_else(|| "В ссылке не указан сервер".to_string())?
        .to_owned();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("Commonnet-Desktop/0.1")
        .redirect(Policy::custom(move |attempt: Attempt<'_>| {
            if attempt.previous().len() >= 5
                || attempt.url().host_str() != Some(original_host.as_str())
                || attempt.url().scheme() != "https"
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| "Не удалось подготовить сетевой запрос".to_string())?;

    let response = client
        .get(subscription_url.clone())
        .send()
        .await
        .map_err(|_| {
            "Не удалось загрузить подписку. Проверь ссылку и подключение к сети".to_string()
        })?;
    if !response.status().is_success() {
        return Err(format!(
            "Сервер подписки ответил с кодом {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SUBSCRIPTION_BYTES as u64)
    {
        return Err("Подписка слишком большая (лимит 2 МБ)".to_string());
    }

    let mut body = Vec::new();
    let mut stream = response;
    while let Some(chunk) = stream
        .chunk()
        .await
        .map_err(|_| "Не удалось прочитать ответ сервера".to_string())?
    {
        if body.len() + chunk.len() > MAX_SUBSCRIPTION_BYTES {
            return Err("Подписка слишком большая (лимит 2 МБ)".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    let text = String::from_utf8(body).map_err(|_| "Подписка должна быть в UTF-8".to_string())?;
    let mut profiles = parse_subscription(&text)?;
    if profiles.is_empty() {
        return Err("В подписке не найдено поддерживаемых профилей".to_string());
    }
    if profiles.len() > 200 {
        return Err("В подписке слишком много профилей (лимит 200)".to_string());
    }

    for (index, profile) in profiles.iter_mut().enumerate() {
        profile.summary.id = format!("profile-{}", index + 1);
    }
    let summaries = profiles
        .iter()
        .map(|profile| profile.summary.clone())
        .collect();
    save_subscription_url(&app, subscription_url.as_str())?;
    let mut current = state
        .profiles
        .lock()
        .map_err(|_| "Не удалось обновить список серверов".to_string())?;
    *current = profiles;
    Ok(summaries)
}

#[tauri::command]
fn saved_subscription_url(app: AppHandle) -> Result<Option<String>, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|_| "Не удалось найти папку настроек Commonnet".to_string())?
        .join("subscription-url");
    let value = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Не удалось прочитать сохранённую ссылку подписки".to_string()),
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    validate_subscription_url(value)?;
    Ok(Some(value.to_string()))
}

fn save_subscription_url(app: &AppHandle, url: &str) -> Result<(), String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|_| "Не удалось найти папку настроек Commonnet".to_string())?;
    fs::create_dir_all(&directory)
        .map_err(|_| "Не удалось сохранить ссылку подписки на устройстве".to_string())?;
    #[cfg(unix)]
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
        .map_err(|_| "Не удалось защитить папку настроек Commonnet".to_string())?;

    let path = directory.join("subscription-url");
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .map_err(|_| "Не удалось сохранить ссылку подписки на устройстве".to_string())?;
    file.write_all(url.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|_| "Не удалось сохранить ссылку подписки на устройстве".to_string())?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|_| "Не удалось защитить сохранённую ссылку подписки".to_string())?;
    Ok(())
}

#[tauri::command]
fn saved_routing_profile(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<RoutingSummary>, String> {
    let profile_path = app
        .path()
        .app_data_dir()
        .map_err(|_| "Не удалось найти папку настроек Commonnet".to_string())?
        .join("routing-profile.json");
    let contents = match fs::read_to_string(profile_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Не удалось прочитать настройки маршрутизации".to_string()),
    };
    let routing: RoutingSettings = serde_json::from_str(&contents)
        .map_err(|_| "Сохранённый профиль маршрутизации повреждён".to_string())?;
    let asset_dir = routing_asset_dir(&app, &routing)?;
    if !asset_dir.join("geoip.dat").is_file() || !asset_dir.join("geosite.dat").is_file() {
        return Err(
            "Не найдены файлы геоданных маршрутизации. Импортируйте профиль ещё раз".to_string(),
        );
    }
    let summary = routing_summary(&routing);
    *state
        .routing
        .lock()
        .map_err(|_| "Не удалось загрузить настройки маршрутизации".to_string())? = Some(routing);
    Ok(Some(summary))
}

#[tauri::command]
async fn import_routing_profile(
    uri: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<RoutingSummary, String> {
    let mut routing = parse_routing_uri(&uri)?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|_| "Не удалось найти папку настроек Commonnet".to_string())?;
    fs::create_dir_all(&app_data)
        .map_err(|_| "Не удалось подготовить папку настроек маршрутизации".to_string())?;
    #[cfg(unix)]
    fs::set_permissions(&app_data, fs::Permissions::from_mode(0o700))
        .map_err(|_| "Не удалось защитить папку настроек маршрутизации".to_string())?;
    let version = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "Не удалось подготовить файлы маршрутизации".to_string())?
            .as_millis()
    );
    routing.asset_version = version;
    let asset_dir = routing_asset_dir(&app, &routing)?;
    fs::create_dir_all(&asset_dir)
        .map_err(|_| "Не удалось подготовить папку геоданных".to_string())?;
    #[cfg(unix)]
    fs::set_permissions(&asset_dir, fs::Permissions::from_mode(0o700))
        .map_err(|_| "Не удалось защитить папку геоданных".to_string())?;

    let assets_result = async {
        let geoip = if routing.geoip_url.is_empty() {
            fs::read(
                engine_path(&app, "geoip.dat")
                    .ok_or_else(|| "В сборке не найден файл geoip.dat".to_string())?,
            )
            .map_err(|_| "Не удалось прочитать geoip.dat".to_string())?
        } else {
            download_routing_asset(&routing.geoip_url).await?
        };
        let geosite = if routing.geosite_url.is_empty() {
            fs::read(
                engine_path(&app, "geosite.dat")
                    .ok_or_else(|| "В сборке не найден файл geosite.dat".to_string())?,
            )
            .map_err(|_| "Не удалось прочитать geosite.dat".to_string())?
        } else {
            download_routing_asset(&routing.geosite_url).await?
        };
        write_private_bytes(&asset_dir.join("geoip.dat"), &geoip)?;
        write_private_bytes(&asset_dir.join("geosite.dat"), &geosite)?;
        validate_routing_with_xray(&app, &routing, &asset_dir)?;
        Ok::<(), String>(())
    }
    .await;
    if let Err(error) = assets_result {
        let _ = fs::remove_dir_all(&asset_dir);
        return Err(error);
    }

    let profile_path = app_data.join("routing-profile.json");
    let serialized = serde_json::to_string(&routing)
        .map_err(|_| "Не удалось сохранить настройки маршрутизации".to_string())?;
    write_private_file(&profile_path, &serialized)?;
    let summary = routing_summary(&routing);
    *state
        .routing
        .lock()
        .map_err(|_| "Не удалось применить настройки маршрутизации".to_string())? = Some(routing);
    Ok(summary)
}

fn routing_summary(routing: &RoutingSettings) -> RoutingSummary {
    RoutingSummary {
        name: routing.name.clone(),
        global_proxy: routing.global_proxy,
        direct_count: routing.direct_sites.len() + routing.direct_ip.len(),
        proxy_count: routing.proxy_sites.len() + routing.proxy_ip.len(),
        block_count: routing.block_sites.len() + routing.block_ip.len(),
    }
}

fn routing_asset_dir(app: &AppHandle, routing: &RoutingSettings) -> Result<PathBuf, String> {
    if routing.asset_version.is_empty()
        || !routing
            .asset_version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        return Err("Профиль содержит неверную версию геоданных".to_string());
    }
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|_| "Не удалось найти папку геоданных".to_string())?
        .join("routing-assets")
        .join(&routing.asset_version))
}

#[tauri::command]
fn list_profiles(state: State<'_, AppState>) -> Result<Vec<ProfileSummary>, String> {
    let profiles = state
        .profiles
        .lock()
        .map_err(|_| "Не удалось прочитать список серверов".to_string())?;
    Ok(profiles
        .iter()
        .map(|profile| profile.summary.clone())
        .collect())
}

#[tauri::command]
fn connection_status(state: State<'_, AppState>) -> Result<ConnectionStatus, String> {
    let active = state
        .active
        .lock()
        .map_err(|_| "Не удалось получить состояние подключения".to_string())?;
    Ok(match active.as_ref() {
        Some(active) => ConnectionStatus {
            connected: true,
            profile_id: Some(active.profile_id.clone()),
            name: Some(active.name.clone()),
            protocol: Some(active.protocol.clone()),
        },
        None => ConnectionStatus {
            connected: false,
            profile_id: None,
            name: None,
            protocol: None,
        },
    })
}

#[tauri::command]
fn connect_profile(
    profile_id: String,
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ConnectionStatus, String> {
    if state
        .active
        .lock()
        .map_err(|_| "Не удалось получить состояние подключения".to_string())?
        .is_some()
    {
        return Err("Сначала отключите текущий сервер".to_string());
    }
    let profile = {
        let profiles = state
            .profiles
            .lock()
            .map_err(|_| "Не удалось прочитать список серверов".to_string())?;
        profiles
            .iter()
            .find(|profile| profile.summary.id == profile_id)
            .cloned()
            .ok_or_else(|| "Профиль не найден. Загрузите подписку ещё раз".to_string())?
    };
    let routing = state
        .routing
        .lock()
        .map_err(|_| "Не удалось прочитать настройки маршрутизации".to_string())?
        .clone();

    let (config_path, process, external_pid) = match profile.summary.protocol.as_str() {
        "AmneziaWG" => {
            if routing.is_some() {
                return Err("Профиль маршрутизации Incy работает с Xray. Выбери сервер Xray для подключения".to_string());
            }
            validate_awg_config(&profile.secret)?;
            (start_platform_awg(&app, &profile)?, None, None)
        }
        "VLESS" | "VMess" | "Trojan" | "Shadowsocks" | "SOCKS" => {
            let config = build_xray_config_with_routing(&profile, routing.as_ref())?;
            let assets = routing
                .as_ref()
                .map(|routing| routing_asset_dir(&app, routing))
                .transpose()?;
            start_platform_xray(&app, &config, assets.as_deref())?
        }
        _ => return Err("Этот протокол не поддерживается".to_string()),
    };
    #[cfg(target_os = "linux")]
    let _ = external_pid;

    let active = ActiveConnection {
        profile_id: profile.summary.id.clone(),
        name: profile.summary.name.clone(),
        protocol: profile.summary.protocol.clone(),
        config_path,
        process,
        #[cfg(not(target_os = "linux"))]
        external_pid,
    };
    let status = ConnectionStatus {
        connected: true,
        profile_id: Some(active.profile_id.clone()),
        name: Some(active.name.clone()),
        protocol: Some(active.protocol.clone()),
    };
    *state
        .active
        .lock()
        .map_err(|_| "Не удалось сохранить состояние подключения".to_string())? = Some(active);
    Ok(status)
}

#[tauri::command]
fn disconnect_profile(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ConnectionStatus, String> {
    let mut active = state
        .active
        .lock()
        .map_err(|_| "Не удалось получить состояние подключения".to_string())?
        .take()
        .ok_or_else(|| "Активное подключение не найдено".to_string())?;

    if let Err(error) = stop_platform_tunnel(&app, &mut active) {
        *state
            .active
            .lock()
            .map_err(|_| "Не удалось восстановить состояние подключения".to_string())? =
            Some(active);
        return Err(error);
    }

    let _ = fs::remove_file(&active.config_path);
    *state
        .active
        .lock()
        .map_err(|_| "Не удалось обновить состояние подключения".to_string())? = None;
    Ok(ConnectionStatus {
        connected: false,
        profile_id: None,
        name: None,
        protocol: None,
    })
}

#[cfg(target_os = "linux")]
fn start_platform_awg(app: &AppHandle, profile: &StoredProfile) -> Result<PathBuf, String> {
    let awg_quick = engine_path(app, "awg-quick").ok_or_else(|| {
        "Не найден awg-quick. Установите AmneziaWG tools и повторите попытку".to_string()
    })?;
    let awg_go = engine_path(app, "amneziawg-go")
        .ok_or_else(|| "Не найден движок AmneziaWG. Повторите установку клиента".to_string())?;
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|_| "Не удалось найти папку данных Commonnet".to_string())?
        .join("tunnels");
    fs::create_dir_all(&directory)
        .map_err(|_| "Не удалось подготовить файл туннеля".to_string())?;
    ensure_linux_tun_device()?;
    let config_path = directory.join(format!("{}.conf", profile.summary.id));
    write_private_file(&config_path, &profile.secret)?;

    let result = Command::new("pkexec")
        .arg("env")
        .arg(format!(
            "WG_QUICK_USERSPACE_IMPLEMENTATION={}",
            awg_go.display()
        ))
        .arg(awg_quick)
        .arg("up")
        .arg(&config_path)
        .output()
        .map_err(|_| "Не удалось запросить права администратора через Polkit".to_string())?;
    if !result.status.success() {
        let _ = fs::remove_file(&config_path);
        return Err(
            if result.status.code() == Some(126) || result.status.code() == Some(127) {
                "Подключение отменено или не получены права администратора".to_string()
            } else {
                "AmneziaWG не смог создать туннель. Проверьте инструменты и конфигурацию"
                    .to_string()
            },
        );
    }
    Ok(config_path)
}

#[cfg(target_os = "linux")]
fn stop_linux_awg(app: &AppHandle, config_path: &std::path::Path) -> Result<(), String> {
    let awg_quick = engine_path(app, "awg-quick")
        .ok_or_else(|| "Не найден awg-quick для остановки туннеля".to_string())?;
    let result = Command::new("pkexec")
        .arg(awg_quick)
        .arg("down")
        .arg(config_path)
        .output()
        .map_err(|_| "Не удалось запросить права для остановки туннеля".to_string())?;
    if !result.status.success() {
        return Err("AmneziaWG не смог остановить туннель".to_string());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn start_platform_xray(
    app: &AppHandle,
    config: &Value,
    asset_dir: Option<&Path>,
) -> Result<(PathBuf, Option<Child>, Option<u32>), String> {
    use std::os::unix::process::CommandExt;
    let xray = engine_path(app, "xray")
        .ok_or_else(|| "Не найден Xray core. Установите Xray и повторите попытку".to_string())?;
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|_| "Не удалось найти папку данных Commonnet".to_string())?
        .join("tunnels");
    fs::create_dir_all(&directory)
        .map_err(|_| "Не удалось подготовить конфигурацию Xray".to_string())?;
    let config_path = directory.join("xray-active.json");
    let serialized = serde_json::to_string_pretty(config)
        .map_err(|_| "Не удалось собрать конфигурацию Xray".to_string())?;
    write_private_file(&config_path, &serialized)?;

    if let Err(error) = validate_xray_outbound(&xray, &directory, config, asset_dir) {
        let _ = fs::remove_file(&config_path);
        return Err(error);
    }

    if let Err(error) = ensure_linux_tun_device() {
        let _ = fs::remove_file(&config_path);
        return Err(error);
    }

    let mut command = Command::new("pkexec");
    command
        .arg("env")
        .arg(format!(
            "XRAY_LOCATION_ASSET={}",
            asset_dir
                .unwrap_or_else(|| xray.parent().unwrap_or(&directory))
                .display()
        ))
        .arg(&xray)
        .args(["run", "-config"])
        .arg(&config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let mut process = command
        .spawn()
        .map_err(|_| "Не удалось запросить права администратора через Polkit".to_string())?;
    for _ in 0..100 {
        thread::sleep(Duration::from_millis(200));
        if let Some(status) = process
            .try_wait()
            .map_err(|_| "Не удалось проверить состояние Xray".to_string())?
        {
            let _ = fs::remove_file(&config_path);
            return Err(
                if status.code() == Some(126) || status.code() == Some(127) {
                    "Запуск Xray отменён или не получены права администратора".to_string()
                } else {
                    "Xray завершился во время запуска туннеля".to_string()
                },
            );
        }
        if Command::new("ip")
            .args(["link", "show", "dev", "commonnet0"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return Ok((config_path, Some(process), None));
        }
    }
    stop_xray_process(&mut process);
    let _ = fs::remove_file(&config_path);
    Err("Xray не создал сетевой интерфейс за 20 секунд".to_string())
}

#[cfg(target_os = "linux")]
fn stop_platform_tunnel(app: &AppHandle, active: &mut ActiveConnection) -> Result<(), String> {
    if let Some(mut process) = active.process.take() {
        stop_xray_process(&mut process);
        let _ = fs::remove_file(&active.config_path);
        return Ok(());
    }
    stop_linux_awg(app, &active.config_path)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn start_platform_awg(app: &AppHandle, profile: &StoredProfile) -> Result<PathBuf, String> {
    let awg_quick = engine_path(app, "awg-quick")
        .ok_or_else(|| "В сборке не найден awg-quick для macOS".to_string())?;
    let awg_go = engine_path(app, "amneziawg-go")
        .ok_or_else(|| "В сборке не найден движок AmneziaWG для macOS".to_string())?;
    let bash = engine_path(app, "bash")
        .ok_or_else(|| "В сборке не найден Bash для AmneziaWG на macOS".to_string())?;
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|_| "Не удалось найти папку данных Commonnet".to_string())?
        .join("tunnels");
    fs::create_dir_all(&directory)
        .map_err(|_| "Не удалось подготовить файл туннеля".to_string())?;
    let config_path = directory.join(format!("{}.conf", profile.summary.id));
    write_private_file(&config_path, &profile.secret)?;
    let command = format!(
        "export WG_QUICK_USERSPACE_IMPLEMENTATION={}; {} {} up {}",
        shell_quote(&awg_go.to_string_lossy()),
        shell_quote(&bash.to_string_lossy()),
        shell_quote(&awg_quick.to_string_lossy()),
        shell_quote(&config_path.to_string_lossy()),
    );
    if let Err(error) = run_macos_admin(&command) {
        let _ = fs::remove_file(&config_path);
        return Err(error);
    }
    Ok(config_path)
}

#[cfg(target_os = "macos")]
fn start_platform_xray(
    app: &AppHandle,
    config: &Value,
    asset_dir: Option<&Path>,
) -> Result<(PathBuf, Option<Child>, Option<u32>), String> {
    let xray = engine_path(app, "xray")
        .ok_or_else(|| "В сборке не найден Xray core для macOS".to_string())?;
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|_| "Не удалось найти папку данных Commonnet".to_string())?
        .join("tunnels");
    fs::create_dir_all(&directory)
        .map_err(|_| "Не удалось подготовить конфигурацию Xray".to_string())?;
    let config_path = directory.join("xray-active.json");
    let serialized = serde_json::to_string_pretty(config)
        .map_err(|_| "Не удалось собрать конфигурацию Xray".to_string())?;
    write_private_file(&config_path, &serialized)?;
    if let Err(error) = validate_xray_outbound(&xray, &directory, config, asset_dir) {
        let _ = fs::remove_file(&config_path);
        return Err(error);
    }

    let log_path = directory.join("xray.log");
    let command = format!(
        "umask 077; before=$(/sbin/ifconfig -l | /usr/bin/tr ' ' '\\n' | /usr/bin/grep '^utun' | /usr/bin/sort | /usr/bin/tr '\\n' ' '); /usr/bin/env XRAY_LOCATION_ASSET={} /usr/bin/nohup {} run -config {} > {} 2>&1 < /dev/null & pid=$!; for i in $(/usr/bin/seq 1 100); do if ! /bin/kill -0 $pid 2>/dev/null; then exit 1; fi; after=$(/sbin/ifconfig -l | /usr/bin/tr ' ' '\\n' | /usr/bin/grep '^utun' | /usr/bin/sort | /usr/bin/tr '\\n' ' '); if [ \"$after\" != \"$before\" ]; then echo $pid; exit 0; fi; /bin/sleep 0.2; done; /bin/kill -TERM $pid 2>/dev/null; exit 1",
        shell_quote(
            &asset_dir
                .unwrap_or_else(|| xray.parent().unwrap_or(&directory))
                .to_string_lossy(),
        ),
        shell_quote(&xray.to_string_lossy()),
        shell_quote(&config_path.to_string_lossy()),
        shell_quote(&log_path.to_string_lossy()),
    );
    let pid = run_macos_admin(&command).and_then(|value| {
        value
            .trim()
            .parse::<u32>()
            .map_err(|_| "Не удалось получить PID Xray".to_string())
    });
    match pid {
        Ok(pid) => Ok((config_path, None, Some(pid))),
        Err(error) => {
            let _ = fs::remove_file(&config_path);
            Err(error)
        }
    }
}

#[cfg(target_os = "macos")]
fn stop_platform_tunnel(app: &AppHandle, active: &mut ActiveConnection) -> Result<(), String> {
    if active.protocol == "AmneziaWG" {
        let awg_quick = engine_path(app, "awg-quick")
            .ok_or_else(|| "В сборке не найден awg-quick для отключения".to_string())?;
        let awg_go = engine_path(app, "amneziawg-go")
            .ok_or_else(|| "В сборке не найден движок AmneziaWG для отключения".to_string())?;
        let bash = engine_path(app, "bash")
            .ok_or_else(|| "В сборке не найден Bash для отключения AmneziaWG".to_string())?;
        let command = format!(
            "export WG_QUICK_USERSPACE_IMPLEMENTATION={}; {} {} down {}",
            shell_quote(&awg_go.to_string_lossy()),
            shell_quote(&bash.to_string_lossy()),
            shell_quote(&awg_quick.to_string_lossy()),
            shell_quote(&active.config_path.to_string_lossy()),
        );
        run_macos_admin(&command)?;
        return Ok(());
    }
    let pid = active
        .external_pid
        .ok_or_else(|| "Не найден процесс Xray для остановки".to_string())?;
    let command = format!(
        "/bin/kill -TERM {} 2>/dev/null || exit 0; for i in $(/usr/bin/seq 1 25); do if ! /bin/kill -0 {} 2>/dev/null; then exit 0; fi; /bin/sleep 0.2; done; exit 1",
        pid, pid
    );
    run_macos_admin(&command).map(|_| ())
}

#[cfg(target_os = "windows")]
fn start_platform_awg(app: &AppHandle, profile: &StoredProfile) -> Result<PathBuf, String> {
    let awg_client = engine_path(app, "amneziawg.exe")
        .ok_or_else(|| "В сборке не найден официальный клиент AmneziaWG для Windows".to_string())?;
    if engine_path(app, "awg.exe").is_none() || engine_path(app, "wintun.dll").is_none() {
        return Err("В сборке отсутствуют компоненты Wintun/awg для Windows".to_string());
    }
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|_| "Не удалось найти папку данных Commonnet".to_string())?
        .join("tunnels");
    fs::create_dir_all(&directory)
        .map_err(|_| "Не удалось подготовить файл туннеля".to_string())?;
    let config_path = directory.join(format!("{}.conf", profile.summary.id));
    write_private_file(&config_path, &profile.secret)?;
    let command = format!(
        "& {} '/installtunnelservice' {}; if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}",
        powershell_literal(&awg_client.to_string_lossy()),
        powershell_literal(&config_path.to_string_lossy()),
    );
    if let Err(error) = run_windows_elevated_script(&command) {
        let _ = fs::remove_file(&config_path);
        return Err(error);
    }
    Ok(config_path)
}

#[cfg(target_os = "windows")]
fn start_platform_xray(
    app: &AppHandle,
    config: &Value,
    asset_dir: Option<&Path>,
) -> Result<(PathBuf, Option<Child>, Option<u32>), String> {
    let xray = engine_path(app, "xray.exe")
        .ok_or_else(|| "В сборке не найден Xray core для Windows".to_string())?;
    let directory = app
        .path()
        .app_local_data_dir()
        .map_err(|_| "Не удалось найти папку данных Commonnet".to_string())?
        .join("tunnels");
    fs::create_dir_all(&directory)
        .map_err(|_| "Не удалось подготовить конфигурацию Xray".to_string())?;
    let config_path = directory.join("xray-active.json");
    let serialized = serde_json::to_string_pretty(config)
        .map_err(|_| "Не удалось собрать конфигурацию Xray".to_string())?;
    write_private_file(&config_path, &serialized)?;
    if let Err(error) = validate_xray_outbound(&xray, &directory, config, asset_dir) {
        let _ = fs::remove_file(&config_path);
        return Err(error);
    }

    let pid_path = directory.join("xray-active.pid");
    let stdout_path = directory.join("xray.log");
    let stderr_path = directory.join("xray-error.log");
    let command = format!(
        "$env:XRAY_LOCATION_ASSET = {}; $proc = Start-Process -FilePath {} -ArgumentList ('run -config ' + {}) -PassThru -WindowStyle Hidden -RedirectStandardOutput {} -RedirectStandardError {}; for ($i=0; $i -lt 100; $i++) {{ Start-Sleep -Milliseconds 200; if (-not (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue)) {{ exit 1 }}; if (Get-NetAdapter -Name 'commonnet0' -ErrorAction SilentlyContinue) {{ [IO.File]::WriteAllText({}, [string]$proc.Id); exit 0 }} }}; Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue; exit 1",
        powershell_literal(
            &asset_dir
                .unwrap_or_else(|| xray.parent().unwrap_or(&directory))
                .to_string_lossy(),
        ),
        powershell_literal(&xray.to_string_lossy()),
        powershell_literal(&powershell_quoted_argument(&config_path.to_string_lossy())),
        powershell_literal(&stdout_path.to_string_lossy()),
        powershell_literal(&stderr_path.to_string_lossy()),
        powershell_literal(&pid_path.to_string_lossy()),
    );
    if let Err(error) = run_windows_elevated_script(&command) {
        let _ = fs::remove_file(&config_path);
        return Err(error);
    }
    let pid = fs::read_to_string(&pid_path)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());
    let _ = fs::remove_file(&pid_path);
    let Some(pid) = pid else {
        let _ = fs::remove_file(&config_path);
        return Err("Xray не создал сетевой интерфейс за 20 секунд".to_string());
    };
    Ok((config_path, None, Some(pid)))
}

#[cfg(target_os = "windows")]
fn stop_platform_tunnel(app: &AppHandle, active: &mut ActiveConnection) -> Result<(), String> {
    if active.protocol == "AmneziaWG" {
        let awg_client = engine_path(app, "amneziawg.exe")
            .ok_or_else(|| "В сборке не найден клиент AmneziaWG для отключения".to_string())?;
        let command = format!(
            "& {} '/uninstalltunnelservice' {}; if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}",
            powershell_literal(&awg_client.to_string_lossy()),
            powershell_literal(&active.profile_id),
        );
        return run_windows_elevated_script(&command).map(|_| ());
    }
    let pid = active
        .external_pid
        .ok_or_else(|| "Не найден процесс Xray для остановки".to_string())?;
    run_windows_elevated_script(&format!(
        "$proc = Get-Process -Id {} -ErrorAction Stop; if ($proc.ProcessName -ne 'xray') {{ exit 1 }}; Stop-Process -Id {} -Force; exit 0",
        pid, pid
    )).map(|_| ())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn start_platform_awg(_app: &AppHandle, _profile: &StoredProfile) -> Result<PathBuf, String> {
    Err("Эта операционная система не поддерживается".to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn start_platform_xray(
    _app: &AppHandle,
    _config: &Value,
    _asset_dir: Option<&Path>,
) -> Result<(PathBuf, Option<Child>, Option<u32>), String> {
    Err("Эта операционная система не поддерживается".to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn stop_platform_tunnel(_app: &AppHandle, _active: &mut ActiveConnection) -> Result<(), String> {
    Err("Эта операционная система не поддерживается".to_string())
}

fn validate_xray_outbound(
    xray: &std::path::Path,
    directory: &std::path::Path,
    config: &Value,
    asset_dir: Option<&Path>,
) -> Result<(), String> {
    let validation_path = directory.join("xray-validation.json");
    let mut validation_config = config.clone();
    validation_config["inbounds"] = serde_json::json!([]);
    let serialized = serde_json::to_string(&validation_config)
        .map_err(|_| "Не удалось проверить конфигурацию Xray".to_string())?;
    write_private_file(&validation_path, &serialized)?;
    let validation = Command::new(xray)
        .args(["run", "-test", "-config"])
        .arg(&validation_path)
        .env(
            "XRAY_LOCATION_ASSET",
            asset_dir.unwrap_or_else(|| xray.parent().unwrap_or(directory)),
        )
        .output();
    let _ = fs::remove_file(&validation_path);
    let validation = validation.map_err(|_| "Не удалось запустить проверку Xray".to_string())?;
    if validation.status.success() {
        Ok(())
    } else {
        Err("Xray отклонил конфигурацию этого сервера".to_string())
    }
}

fn validate_routing_with_xray(
    app: &AppHandle,
    routing: &RoutingSettings,
    asset_dir: &Path,
) -> Result<(), String> {
    let xray_name = if cfg!(target_os = "windows") {
        "xray.exe"
    } else {
        "xray"
    };
    let xray =
        engine_path(app, xray_name).ok_or_else(|| "В сборке не найден Xray core".to_string())?;
    let mut config = serde_json::json!({
        "log": {"loglevel": "warning"},
        "dns": {"servers": []},
        "inbounds": [],
        "outbounds": [{"tag": "proxy", "protocol": "freedom"}]
    });
    apply_routing_settings(&mut config, routing)?;
    let path = std::env::temp_dir().join(format!(
        "commonnet-routing-check-{}-{}.json",
        std::process::id(),
        routing.asset_version
    ));
    let serialized = serde_json::to_string_pretty(&config)
        .map_err(|_| "Не удалось проверить правила маршрутизации".to_string())?;
    write_private_file(&path, &serialized)?;
    let result = Command::new(xray)
        .args(["run", "-test", "-config"])
        .arg(&path)
        .env("XRAY_LOCATION_ASSET", asset_dir)
        .output();
    let _ = fs::remove_file(path);
    let result = result.map_err(|_| "Не удалось проверить правила в Xray".to_string())?;
    if result.status.success() {
        Ok(())
    } else {
        Err("Xray отклонил правила маршрутизации или файлы геоданных".to_string())
    }
}

#[cfg(unix)]
fn write_private_bytes(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "Не удалось сохранить файл геоданных".to_string())?;
    file.write_all(contents)
        .map_err(|_| "Не удалось сохранить файл геоданных".to_string())?;
    Ok(())
}

#[cfg(windows)]
fn write_private_bytes(path: &Path, contents: &[u8]) -> Result<(), String> {
    fs::write(path, contents).map_err(|_| "Не удалось сохранить файл геоданных".to_string())?;
    let username = std::env::var("USERNAME")
        .map_err(|_| "Не удалось ограничить доступ к геоданным".to_string())?;
    let acl = Command::new("icacls.exe")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{username}:(F)"))
        .arg("SYSTEM:(F)")
        .output()
        .map_err(|_| "Не удалось защитить файл геоданных".to_string())?;
    if !acl.status.success() {
        let _ = fs::remove_file(path);
        return Err("Не удалось защитить файл геоданных".to_string());
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn write_private_bytes(path: &Path, contents: &[u8]) -> Result<(), String> {
    fs::write(path, contents).map_err(|_| "Не удалось сохранить файл геоданных".to_string())
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "macos")]
fn run_macos_admin(command: &str) -> Result<String, String> {
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("do shell script \"{escaped}\" with administrator privileges");
    let output = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|_| "Не удалось открыть запрос прав администратора macOS".to_string())?;
    if !output.status.success() {
        return Err("Операция отменена или macOS не предоставила права администратора".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(target_os = "windows")]
fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(target_os = "windows")]
fn powershell_quoted_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "`\""))
}

#[cfg(target_os = "windows")]
fn encode_powershell(script: &str) -> String {
    let utf16 = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    base64::engine::general_purpose::STANDARD.encode(utf16)
}

#[cfg(target_os = "windows")]
fn run_windows_elevated_script(script: &str) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    let encoded_inner = encode_powershell(script);
    let outer = format!(
        "$p = Start-Process -FilePath 'powershell.exe' -ArgumentList '-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {encoded_inner}' -Verb RunAs -Wait -PassThru -WindowStyle Hidden; exit $p.ExitCode"
    );
    let encoded_outer = encode_powershell(&outer);
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encoded_outer,
        ])
        .creation_flags(0x08000000)
        .output()
        .map_err(|_| "Не удалось запросить права администратора Windows".to_string())?;
    if !output.status.success() {
        return Err(
            "Операция отменена или Windows не предоставила права администратора".to_string(),
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(target_os = "linux")]
fn stop_xray_process(process: &mut Child) {
    let process_group = -(process.id() as i32);
    unsafe {
        libc::kill(process_group, libc::SIGTERM);
    }
    let _ = process.wait();
}

#[cfg(target_os = "linux")]
fn ensure_linux_tun_device() -> Result<(), String> {
    use std::os::unix::fs::FileTypeExt;
    let tun_device = std::path::Path::new("/dev/net/tun");
    if tun_device.exists() {
        return if fs::metadata(tun_device)
            .is_ok_and(|metadata| metadata.file_type().is_char_device())
        {
            Ok(())
        } else {
            Err("/dev/net/tun существует, но не является TUN-устройством".to_string())
        };
    }
    let result = Command::new("pkexec")
        .args([
            "sh",
            "-c",
            "modprobe tun && mkdir -p /dev/net && if [ ! -e /dev/net/tun ]; then mknod /dev/net/tun c 10 200; fi && chmod 600 /dev/net/tun",
        ])
        .output()
        .map_err(|_| "Не удалось запросить права для настройки TUN-устройства".to_string())?;
    if !result.status.success()
        || !fs::metadata(tun_device).is_ok_and(|metadata| metadata.file_type().is_char_device())
    {
        return Err("Не удалось подготовить /dev/net/tun. Проверьте модуль ядра TUN".to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn write_private_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "Не удалось записать конфигурацию туннеля".to_string())?;
    file.write_all(contents.as_bytes())
        .map_err(|_| "Не удалось сохранить конфигурацию туннеля".to_string())
}

#[cfg(windows)]
fn write_private_file(path: &std::path::Path, contents: &str) -> Result<(), String> {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|_| "Не удалось записать конфигурацию туннеля".to_string())?;
    file.write_all(contents.as_bytes())
        .map_err(|_| "Не удалось сохранить конфигурацию туннеля".to_string())?;
    let username = std::env::var("USERNAME")
        .map_err(|_| "Не удалось ограничить доступ к конфигурации".to_string())?;
    let acl = Command::new("icacls.exe")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{username}:(F)"))
        .arg("SYSTEM:(F)")
        .output()
        .map_err(|_| "Не удалось защитить конфигурацию VPN".to_string())?;
    if !acl.status.success() {
        let _ = fs::remove_file(path);
        return Err("Не удалось защитить конфигурацию VPN".to_string());
    }
    Ok(())
}

fn engine_path(app: &AppHandle, name: &str) -> Option<PathBuf> {
    let packaged = app
        .path()
        .resolve(
            PathBuf::from("resources/engines").join(name),
            tauri::path::BaseDirectory::Resource,
        )
        .ok();
    let development = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("resources/engines")
        .join(name);
    packaged
        .filter(|path| path.is_file())
        .or_else(|| development.is_file().then_some(development))
        .or_else(|| find_executable(name))
}

#[tauri::command]
fn runtime_status(app: AppHandle) -> RuntimeStatus {
    let platform = if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "неизвестная ОС"
    };

    let xray_name = if cfg!(target_os = "windows") {
        "xray.exe"
    } else {
        "xray"
    };
    let awg_detected = if cfg!(target_os = "windows") {
        ["amneziawg.exe", "awg.exe", "wintun.dll"]
            .iter()
            .all(|name| engine_path(&app, name).is_some())
    } else if cfg!(target_os = "macos") {
        ["awg-quick", "awg", "amneziawg-go", "bash"]
            .iter()
            .all(|name| engine_path(&app, name).is_some())
    } else {
        ["awg-quick", "awg", "amneziawg-go"]
            .iter()
            .all(|name| engine_path(&app, name).is_some())
    };

    RuntimeStatus {
        platform: platform.to_string(),
        xray_detected: engine_path(&app, xray_name).is_some(),
        awg_detected,
        note: match platform {
            "Linux" => {
                "Движки поставляются вместе с приложением. Для подключения появится запрос Polkit."
                    .to_string()
            }
            "macOS" => {
                "Движки поставляются вместе с приложением. macOS запросит пароль администратора."
                    .to_string()
            }
            "Windows" => {
                "Движки поставляются вместе с приложением. Windows запросит подтверждение UAC."
                    .to_string()
            }
            _ => "Эта операционная система не поддерживается.".to_string(),
        },
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let search_name = if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(&search_name))
        .find(|path| path.is_file())
}

fn validate_subscription_url(raw: &str) -> Result<Url, String> {
    validate_public_https_url(raw, "Ссылка на подписку")
}

fn validate_public_https_url(raw: &str, description: &str) -> Result<Url, String> {
    let parsed =
        Url::parse(raw.trim()).map_err(|_| format!("{description}: неверный HTTPS-адрес"))?;
    if parsed.scheme() != "https" {
        return Err(format!("{description} должна использовать HTTPS"));
    }
    if parsed.username() != "" || parsed.password().is_some() {
        return Err(format!("{description} не должна содержать логин и пароль"));
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return Err(format!(
            "{description} не может указывать на локальный адрес"
        ));
    }
    if let Some(address) = parsed.host().and_then(|host| match host {
        url::Host::Ipv4(address) => Some(std::net::IpAddr::V4(address)),
        url::Host::Ipv6(address) => Some(std::net::IpAddr::V6(address)),
        url::Host::Domain(_) => None,
    }) {
        let is_private = match address {
            std::net::IpAddr::V4(ip) => {
                ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
                    || ip.is_unspecified()
            }
        };
        if is_private {
            return Err(format!(
                "{description} не может указывать на локальный адрес"
            ));
        }
    }
    Ok(parsed)
}

fn parse_routing_uri(raw: &str) -> Result<RoutingSettings, String> {
    let uri = Url::parse(raw.trim()).map_err(|_| "Вставь ссылку маршрутизации Incy".to_string())?;
    let segments = uri
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    if uri.scheme() != "incy"
        || uri.host_str() != Some("routing")
        || segments.len() != 2
        || segments[0] != "onadd"
    {
        return Err("Нужна ссылка формата incy://routing/onadd/<профиль>".to_string());
    }
    let payload = percent_decode(segments[1]);
    let json = decode_base64(&payload)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or_else(|| "Не удалось декодировать профиль маршрутизации".to_string())?;
    let value: Value = serde_json::from_str(&json)
        .map_err(|_| "Профиль маршрутизации содержит неверный JSON".to_string())?;

    let name = value
        .get("Name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty() && name.len() <= 80)
        .unwrap_or("Маршрутизация Incy")
        .trim()
        .to_string();
    let global_proxy = routing_bool(&value, "GlobalProxy", true)?;
    let fake_dns = routing_bool(&value, "FakeDNS", false)?;
    if fake_dns {
        return Err("Профили Incy с FakeDNS пока не поддерживаются".to_string());
    }
    let route_order = value
        .get("RouteOrder")
        .and_then(Value::as_str)
        .unwrap_or("block-proxy-direct")
        .to_ascii_lowercase();
    let route_parts = route_order.split('-').collect::<Vec<_>>();
    if route_parts.len() != 3
        || ["block", "proxy", "direct"]
            .iter()
            .any(|name| !route_parts.contains(name))
    {
        return Err("RouteOrder должен содержать block, proxy и direct".to_string());
    }
    let domain_strategy = value
        .get("DomainStrategy")
        .and_then(Value::as_str)
        .unwrap_or("IPIfNonMatch")
        .to_string();
    if !["AsIs", "IPIfNonMatch", "IPOnDemand"].contains(&domain_strategy.as_str()) {
        return Err("В профиле указан неподдерживаемый DomainStrategy".to_string());
    }

    let direct_sites = routing_list(&value, "DirectSites", true)?;
    let direct_ip = routing_list(&value, "DirectIp", false)?;
    let proxy_sites = routing_list(&value, "ProxySites", true)?;
    let proxy_ip = routing_list(&value, "ProxyIp", false)?;
    let block_sites = routing_list(&value, "BlockSites", true)?;
    let block_ip = routing_list(&value, "BlockIp", false)?;

    let mut dns_hosts = HashMap::new();
    if let Some(hosts) = value.get("DnsHosts") {
        let hosts = hosts
            .as_object()
            .ok_or_else(|| "DnsHosts должен быть объектом".to_string())?;
        for (domain, address) in hosts {
            let address = address
                .as_str()
                .ok_or_else(|| "Адрес в DnsHosts должен быть строкой".to_string())?;
            if !is_valid_domain_name(domain) || address.parse::<IpAddr>().is_err() {
                return Err("DnsHosts содержит неверный домен или IP-адрес".to_string());
            }
            dns_hosts.insert(domain.clone(), address.to_string());
        }
    }

    let remote_dns_type = routing_string(&value, "RemoteDnsType", "DNS")?;
    let domestic_dns_type = routing_string(&value, "DomesticDnsType", "DNS")?;
    validate_dns_type(&remote_dns_type)?;
    validate_dns_type(&domestic_dns_type)?;
    let remote_dns_domain = routing_string(&value, "RemoteDnsDomain", "")?;
    let domestic_dns_domain = routing_string(&value, "DomesticDnsDomain", "")?;
    if remote_dns_type.eq_ignore_ascii_case("doh") && !remote_dns_domain.is_empty() {
        validate_public_https_url(&remote_dns_domain, "RemoteDnsDomain")?;
    }
    if domestic_dns_type.eq_ignore_ascii_case("doh") && !domestic_dns_domain.is_empty() {
        validate_public_https_url(&domestic_dns_domain, "DomesticDnsDomain")?;
    }
    let remote_dns = routing_string(&value, "RemoteDns", "1.1.1.1")?;
    let domestic_dns = routing_string(&value, "DomesticDns", "8.8.8.8")?;
    validate_dns_address(&remote_dns)?;
    validate_dns_address(&domestic_dns)?;
    let geoip_url = routing_string(&value, "Geoipurl", "")?;
    let geosite_url = routing_string(&value, "Geositeurl", "")?;
    if !geoip_url.is_empty() {
        validate_public_https_url(&geoip_url, "Geoipurl")?;
    }
    if !geosite_url.is_empty() {
        validate_public_https_url(&geosite_url, "Geositeurl")?;
    }

    Ok(RoutingSettings {
        name,
        global_proxy,
        remote_dns,
        domestic_dns,
        remote_dns_type,
        domestic_dns_type,
        remote_dns_domain,
        domestic_dns_domain,
        dns_hosts,
        route_order,
        direct_sites,
        direct_ip,
        proxy_sites,
        proxy_ip,
        block_sites,
        block_ip,
        domain_strategy,
        geoip_url,
        geosite_url,
        asset_version: String::new(),
    })
}

fn routing_string(value: &Value, field: &str, fallback: &str) -> Result<String, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(fallback.to_string()),
        Some(Value::String(value)) => Ok(value.trim().to_string()),
        _ => Err(format!("{field} должен быть строкой")),
    }
}

fn routing_bool(value: &Value, field: &str, fallback: bool) -> Result<bool, String> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(fallback),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::String(value)) if value.eq_ignore_ascii_case("true") => Ok(true),
        Some(Value::String(value)) if value.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(format!("{field} должен быть true или false")),
    }
}

fn routing_list(value: &Value, field: &str, domain_list: bool) -> Result<Vec<String>, String> {
    let Some(list) = value.get(field) else {
        return Ok(Vec::new());
    };
    let list = list
        .as_array()
        .ok_or_else(|| format!("{field} должен быть списком"))?;
    let mut output = Vec::with_capacity(list.len());
    for item in list {
        let item = item
            .as_str()
            .ok_or_else(|| format!("Элементы {field} должны быть строками"))?
            .trim();
        let valid = if domain_list {
            ["geosite:", "domain:", "full:", "keyword:", "regexp:"]
                .iter()
                .any(|prefix| {
                    item.strip_prefix(prefix)
                        .is_some_and(|suffix| !suffix.is_empty())
                })
        } else {
            item.strip_prefix("geoip:")
                .is_some_and(|suffix| !suffix.is_empty())
                || valid_ip_range(item)
                || item
                    .strip_prefix("ext:geoip.dat:")
                    .is_some_and(|suffix| !suffix.is_empty())
        };
        if !valid || item.len() > 512 || item.chars().any(char::is_control) {
            return Err(format!("В {field} найдено неподдерживаемое правило"));
        }
        output.push(item.to_string());
    }
    Ok(output)
}

fn valid_ip_range(value: &str) -> bool {
    if let Some((address, prefix)) = value.split_once('/') {
        let (Ok(address), Ok(prefix)) = (address.parse::<IpAddr>(), prefix.parse::<u8>()) else {
            return false;
        };
        prefix <= if address.is_ipv4() { 32 } else { 128 }
    } else {
        value.parse::<IpAddr>().is_ok()
    }
}

fn is_valid_domain_name(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 || value.chars().any(char::is_control) {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn validate_dns_type(value: &str) -> Result<(), String> {
    if value.eq_ignore_ascii_case("doh")
        || value.eq_ignore_ascii_case("dns")
        || value.eq_ignore_ascii_case("udp")
    {
        Ok(())
    } else {
        Err("Поддерживаются DNS-типы DoH и UDP".to_string())
    }
}

fn validate_dns_address(value: &str) -> Result<(), String> {
    let valid = value.parse::<IpAddr>().is_ok()
        || value.parse::<std::net::SocketAddr>().is_ok()
        || is_valid_domain_name(value)
        || value.split_once(':').is_some_and(|(host, port)| {
            is_valid_domain_name(host) && port.parse::<u16>().is_ok_and(|port| port > 0)
        });
    if valid {
        Ok(())
    } else {
        Err("В настройках маршрутизации указан неверный DNS-адрес".to_string())
    }
}

async fn download_routing_asset(raw_url: &str) -> Result<Vec<u8>, String> {
    const MAX_GEODATA_BYTES: usize = 64 * 1024 * 1024;
    let url = validate_public_https_url(raw_url, "URL геоданных")?;
    let original_host = url
        .host_str()
        .ok_or_else(|| "В URL геоданных не указан сервер".to_string())?
        .to_owned();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent("Commonnet-Desktop/0.1.4")
        .redirect(Policy::custom(move |attempt: Attempt<'_>| {
            if attempt.previous().len() >= 3
                || attempt.url().host_str() != Some(original_host.as_str())
                || attempt.url().scheme() != "https"
            {
                attempt.stop()
            } else {
                attempt.follow()
            }
        }))
        .build()
        .map_err(|_| "Не удалось подготовить загрузку геоданных".to_string())?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "Не удалось загрузить геоданные маршрутизации".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "Сервер геоданных ответил с кодом {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_GEODATA_BYTES as u64)
    {
        return Err("Файл геоданных превышает лимит 64 МБ".to_string());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Не удалось прочитать файл геоданных".to_string())?
    {
        if bytes.len() + chunk.len() > MAX_GEODATA_BYTES {
            return Err("Файл геоданных превышает лимит 64 МБ".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err("Сервер вернул пустой файл геоданных".to_string());
    }
    Ok(bytes)
}

fn parse_subscription(raw: &str) -> Result<Vec<StoredProfile>, String> {
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        if let Some(items) = value.as_array() {
            let mut results = Vec::new();
            for item in items {
                let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
                let servers = item.get("servers").and_then(Value::as_array);
                if kind.to_ascii_lowercase().contains("amneziawg") {
                    if let Some(servers) = servers {
                        for server in servers {
                            let name = server
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("AmneziaWG сервер");
                            let encoded =
                                server
                                    .get("config")
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| {
                                        "В профиле AmneziaWG отсутствует конфигурация".to_string()
                                    })?;
                            let config = decode_base64(encoded).ok_or_else(|| {
                                "Не удалось декодировать конфигурацию AmneziaWG".to_string()
                            })?;
                            let config = String::from_utf8(config).map_err(|_| {
                                "Конфигурация AmneziaWG имеет неверную кодировку".to_string()
                            })?;
                            validate_awg_config(&config)?;
                            results.push(make_profile(
                                name,
                                "AmneziaWG",
                                awg_endpoint(&config),
                                config,
                            ));
                        }
                    }
                }
            }
            if !results.is_empty() {
                return Ok(results);
            }
        }
    }

    let decoded = decode_base64(raw.trim()).and_then(|bytes| String::from_utf8(bytes).ok());
    let lines = decoded.as_deref().unwrap_or(raw);
    let mut profiles = Vec::new();
    for (index, line) in lines.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(profile) = parse_xray_profile(line, index) {
            profiles.push(profile);
        }
    }
    Ok(profiles)
}

fn parse_xray_profile(line: &str, index: usize) -> Option<StoredProfile> {
    let (protocol, endpoint, name) = if line.starts_with("vmess://") {
        let raw = decode_base64(line.trim_start_matches("vmess://"))?;
        let json: Value = serde_json::from_slice(&raw).ok()?;
        let host = json.get("add")?.as_str()?;
        let port = json
            .get("port")?
            .as_u64()
            .or_else(|| json.get("port")?.as_str()?.parse::<u64>().ok())?;
        let name = json
            .get("ps")
            .and_then(Value::as_str)
            .unwrap_or("VMess сервер");
        ("VMess", format!("{host}:{port}"), name.to_string())
    } else {
        let url = Url::parse(line).ok()?;
        let protocol = match url.scheme() {
            "vless" => "VLESS",
            "trojan" => "Trojan",
            "ss" => "Shadowsocks",
            "socks" | "socks5" => "SOCKS",
            _ => return None,
        };
        let host = url.host_str()?;
        let port = url.port().unwrap_or_else(|| match url.scheme() {
            "ss" => 8388,
            "socks" | "socks5" => 1080,
            _ => 443,
        });
        let name = url
            .fragment()
            .map(|fragment| percent_decode(fragment))
            .filter(|fragment| !fragment.is_empty())
            .unwrap_or_else(|| format!("{protocol} сервер"));
        (protocol, format!("{host}:{port}"), name)
    };
    Some(make_profile(&name, protocol, endpoint, line.to_string()).with_id(index + 1))
}

#[cfg(test)]
fn build_xray_config(profile: &StoredProfile) -> Result<Value, String> {
    build_xray_config_with_routing(profile, None)
}

fn build_xray_config_with_routing(
    profile: &StoredProfile,
    routing: Option<&RoutingSettings>,
) -> Result<Value, String> {
    let outbound = build_xray_outbound(&profile.secret, &profile.summary.protocol)?;
    let platform = if cfg!(target_os = "macos") {
        "utun"
    } else {
        "commonnet0"
    };
    let mut config = serde_json::json!({
        "log": {"loglevel": "warning"},
        "dns": {"servers": ["https://1.1.1.1/dns-query", "https://8.8.8.8/dns-query"]},
        "inbounds": [{
            "tag": "commonnet-tun",
            "protocol": "tun",
            "settings": {
                "name": platform,
                "desc": "Commonnet",
                "mtu": 1500,
                "gateway": ["10.255.0.1/30", "fd00:434e:4554::1/126"],
                "dns": ["10.255.0.2", "fd00:434e:4554::2"],
                "autoSystemRoutingTable": ["0.0.0.0/0", "::/0"],
                "autoOutboundsInterface": "auto",
                "autoSystemDnsToGateway": cfg!(target_os = "linux"),
                "autoSystemWfpBlockLeak": if cfg!(target_os = "windows") {
                    serde_json::json!(["dns", "misconfigtun"])
                } else {
                    serde_json::json!([])
                }
            }
        }],
        "outbounds": [outbound]
    });
    if let Some(routing) = routing {
        apply_routing_settings(&mut config, routing)?;
    }
    Ok(config)
}

fn apply_routing_settings(config: &mut Value, routing: &RoutingSettings) -> Result<(), String> {
    let remote_dns = if routing.remote_dns_type.eq_ignore_ascii_case("doh")
        && !routing.remote_dns_domain.is_empty()
    {
        routing.remote_dns_domain.as_str()
    } else {
        routing.remote_dns.as_str()
    };
    let domestic_dns = if routing.domestic_dns_type.eq_ignore_ascii_case("doh")
        && !routing.domestic_dns_domain.is_empty()
    {
        routing.domestic_dns_domain.as_str()
    } else {
        routing.domestic_dns.as_str()
    };
    let mut dns_servers = Vec::new();
    if !routing.direct_sites.is_empty() {
        dns_servers.push(serde_json::json!({
            "address": domestic_dns,
            "domains": routing.direct_sites,
            "skipFallback": true
        }));
    }
    if !routing.proxy_sites.is_empty() {
        dns_servers.push(serde_json::json!({
            "address": remote_dns,
            "domains": routing.proxy_sites,
            "skipFallback": true
        }));
    }
    dns_servers.push(Value::String(if routing.global_proxy {
        remote_dns.to_string()
    } else {
        domestic_dns.to_string()
    }));
    config["dns"] = serde_json::json!({
        "servers": dns_servers,
        "hosts": routing.dns_hosts
    });

    if let Some(inbound) = config
        .get_mut("inbounds")
        .and_then(Value::as_array_mut)
        .and_then(|inbounds| inbounds.first_mut())
    {
        inbound["sniffing"] = serde_json::json!({
            "enabled": true,
            "destOverride": ["http", "tls", "quic"],
            "routeOnly": true
        });
    }

    let proxy = config["outbounds"]
        .as_array()
        .and_then(|outbounds| outbounds.first())
        .cloned()
        .ok_or_else(|| "Не удалось собрать прокси для маршрутизации".to_string())?;
    let direct = serde_json::json!({"tag": "direct", "protocol": "freedom", "settings": {}});
    let block = serde_json::json!({"tag": "block", "protocol": "blackhole", "settings": {}});
    config["outbounds"] = if routing.global_proxy {
        serde_json::json!([proxy, direct, block])
    } else {
        serde_json::json!([direct, proxy, block])
    };

    let mut rules = Vec::new();
    for category in routing.route_order.split('-') {
        let (tag, domains, ips) = match category {
            "block" => ("block", &routing.block_sites, &routing.block_ip),
            "proxy" => ("proxy", &routing.proxy_sites, &routing.proxy_ip),
            "direct" => ("direct", &routing.direct_sites, &routing.direct_ip),
            _ => return Err("Неизвестный тип правила маршрутизации".to_string()),
        };
        if !domains.is_empty() {
            rules.push(serde_json::json!({
                "type": "field",
                "domain": domains,
                "outboundTag": tag
            }));
        }
        if !ips.is_empty() {
            rules.push(serde_json::json!({
                "type": "field",
                "ip": ips,
                "outboundTag": tag
            }));
        }
    }
    config["routing"] = serde_json::json!({
        "domainStrategy": routing.domain_strategy,
        "rules": rules
    });
    Ok(())
}

fn build_xray_outbound(uri: &str, display_protocol: &str) -> Result<Value, String> {
    if display_protocol == "VMess" {
        return build_vmess_outbound(uri);
    }
    let url = Url::parse(uri).map_err(|_| "Xray-ссылка повреждена".to_string())?;
    let host = url
        .host_str()
        .ok_or_else(|| "В Xray-ссылке не указан сервер".to_string())?;
    let port = url
        .port()
        .or_else(|| {
            Some(match url.scheme() {
                "ss" => 8388,
                "socks" | "socks5" => 1080,
                _ => 443,
            })
        })
        .ok_or_else(|| "В Xray-ссылке не указан порт".to_string())?;
    let query = url
        .query_pairs()
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();
    let stream = build_stream_settings(&query)?;
    let protocol = match display_protocol {
        "VLESS" => "vless",
        "Trojan" => "trojan",
        "Shadowsocks" => "shadowsocks",
        "SOCKS" => "socks",
        _ => return Err("Этот Xray-протокол не поддерживается".to_string()),
    };
    let settings = match protocol {
        "vless" => {
            let user_id = percent_decode(url.username());
            if user_id.is_empty() {
                return Err("В VLESS-ссылке не указан UUID".to_string());
            }
            let mut user = serde_json::json!({
                "id": user_id,
                "encryption": query.get("encryption").map(String::as_str).unwrap_or("none")
            });
            if let Some(flow) = query.get("flow").filter(|flow| !flow.is_empty()) {
                user["flow"] = Value::String(flow.clone());
            }
            serde_json::json!({"vnext": [{"address": host, "port": port, "users": [user]}]})
        }
        "trojan" => {
            let password = percent_decode(url.username());
            if password.is_empty() {
                return Err("В Trojan-ссылке не указан пароль".to_string());
            }
            serde_json::json!({"servers": [{"address": host, "port": port, "password": password}]})
        }
        "shadowsocks" => {
            let (method, password) = shadowsocks_credentials(&url)?;
            serde_json::json!({"servers": [{"address": host, "port": port, "method": method, "password": password}]})
        }
        "socks" => {
            let mut server = serde_json::json!({"address": host, "port": port});
            if !url.username().is_empty() {
                server["users"] = serde_json::json!([{
                    "user": percent_decode(url.username()),
                    "pass": url.password().map(percent_decode).unwrap_or_default()
                }]);
            }
            serde_json::json!({"servers": [server]})
        }
        _ => unreachable!(),
    };
    Ok(
        serde_json::json!({"tag": "proxy", "protocol": protocol, "settings": settings, "streamSettings": stream}),
    )
}

fn build_vmess_outbound(uri: &str) -> Result<Value, String> {
    let encoded = uri
        .strip_prefix("vmess://")
        .ok_or_else(|| "Неверная VMess-ссылка".to_string())?;
    let raw =
        decode_base64(encoded).ok_or_else(|| "Не удалось декодировать VMess-ссылку".to_string())?;
    let vmess: Value = serde_json::from_slice(&raw)
        .map_err(|_| "VMess-конфигурация имеет неверный JSON".to_string())?;
    let host = vmess
        .get("add")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "В VMess-ссылке не указан сервер".to_string())?;
    let port = vmess
        .get("port")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .ok_or_else(|| "В VMess-ссылке не указан порт".to_string())?;
    let id = vmess
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| "В VMess-ссылке не указан UUID".to_string())?;
    let alter_id = vmess
        .get("aid")
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
        .unwrap_or(0);
    let cipher = vmess
        .get("scy")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or("auto");
    let mut query = std::collections::HashMap::new();
    for (source, target) in [
        ("net", "type"),
        ("tls", "security"),
        ("sni", "sni"),
        ("host", "host"),
        ("path", "path"),
        ("fp", "fp"),
        ("serviceName", "serviceName"),
    ] {
        if let Some(value) = vmess.get(source).and_then(Value::as_str) {
            if !value.is_empty() && value != "none" {
                query.insert(target.to_string(), value.to_string());
            }
        }
    }
    let stream = build_stream_settings(&query)?;
    Ok(serde_json::json!({
        "tag": "proxy",
        "protocol": "vmess",
        "settings": {"vnext": [{"address": host, "port": port, "users": [{"id": id, "alterId": alter_id, "security": cipher}]}]},
        "streamSettings": stream
    }))
}

fn build_stream_settings(
    query: &std::collections::HashMap<String, String>,
) -> Result<Value, String> {
    let network = query.get("type").map(String::as_str).unwrap_or("tcp");
    let security = query.get("security").map(String::as_str).unwrap_or("none");
    let stream_security = match security {
        "none" | "" => Value::Null,
        "tls" => {
            let mut tls = serde_json::json!({"allowInsecure": false});
            set_if_present(&mut tls, "serverName", query.get("sni"));
            set_if_present(&mut tls, "fingerprint", query.get("fp"));
            if let Some(alpn) = query.get("alpn") {
                tls["alpn"] = Value::Array(
                    alpn.split(',')
                        .map(|v| Value::String(v.trim().to_string()))
                        .collect(),
                );
            }
            serde_json::json!({"security": "tls", "tlsSettings": tls})
        }
        "reality" => {
            let mut reality = serde_json::json!({"show": false});
            set_if_present(&mut reality, "serverName", query.get("sni"));
            set_if_present(&mut reality, "fingerprint", query.get("fp"));
            set_if_present(&mut reality, "password", query.get("pbk"));
            set_if_present(&mut reality, "shortId", query.get("sid"));
            set_if_present(&mut reality, "spiderX", query.get("spx"));
            if reality.get("password").is_none() {
                return Err("В REALITY-ссылке отсутствует публичный ключ".to_string());
            }
            serde_json::json!({"security": "reality", "realitySettings": reality})
        }
        _ => return Err(format!("Защита Xray {security} пока не поддерживается")),
    };

    let transport = match network {
        "tcp" | "raw" => Value::Null,
        "ws" => {
            let mut settings =
                serde_json::json!({"path": query.get("path").map(String::as_str).unwrap_or("/")});
            if let Some(host) = query.get("host") {
                settings["headers"] = serde_json::json!({"Host": host});
            }
            serde_json::json!({"wsSettings": settings})
        }
        "grpc" => serde_json::json!({"grpcSettings": {
            "serviceName": query.get("serviceName").map(String::as_str).unwrap_or(""),
            "multiMode": query.get("mode").is_some_and(|mode| mode == "multi")
        }}),
        "httpupgrade" => serde_json::json!({"httpupgradeSettings": {
            "host": query.get("host").map(String::as_str).unwrap_or(""),
            "path": query.get("path").map(String::as_str).unwrap_or("/")
        }}),
        "xhttp" | "splithttp" => serde_json::json!({"splithttpSettings": {
            "host": query.get("host").map(String::as_str).unwrap_or(""),
            "path": query.get("path").map(String::as_str).unwrap_or("/"),
            "mode": query.get("mode").map(String::as_str).unwrap_or("auto")
        }}),
        _ => return Err(format!("Транспорт Xray {network} пока не поддерживается")),
    };
    let mut stream = serde_json::json!({"network": if network == "raw" { "tcp" } else if network == "xhttp" { "splithttp" } else { network }});
    if !stream_security.is_null() {
        if let Some(object) = stream_security.as_object() {
            for (key, value) in object {
                stream[key] = value.clone();
            }
        }
    }
    if let Some(object) = transport.as_object() {
        for (key, value) in object {
            stream[key] = value.clone();
        }
    }
    Ok(stream)
}

fn set_if_present(target: &mut Value, key: &str, value: Option<&String>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        target[key] = Value::String(value.clone());
    }
}

fn shadowsocks_credentials(url: &Url) -> Result<(String, String), String> {
    let username = percent_decode(url.username());
    let password = url.password().map(percent_decode);
    if let Some(password) = password {
        return Ok((username, password));
    }
    if let Some(decoded) = decode_base64(&username).and_then(|value| String::from_utf8(value).ok())
    {
        if let Some((method, password)) = decoded.split_once(':') {
            return Ok((method.to_string(), password.to_string()));
        }
    }
    Err("В Shadowsocks-ссылке не удалось прочитать метод и пароль".to_string())
}

fn make_profile(name: &str, protocol: &str, endpoint: String, secret: String) -> StoredProfile {
    let safe_name = name.trim().chars().take(64).collect::<String>();
    StoredProfile {
        summary: ProfileSummary {
            id: "pending".to_string(),
            name: if safe_name.is_empty() {
                "Сервер".to_string()
            } else {
                safe_name
            },
            protocol: protocol.to_string(),
            endpoint,
        },
        secret,
    }
}

trait ProfileId {
    fn with_id(self, number: usize) -> Self;
}

impl ProfileId for StoredProfile {
    fn with_id(mut self, number: usize) -> Self {
        self.summary.id = format!("profile-{number}");
        self
    }
}

fn validate_awg_config(config: &str) -> Result<(), String> {
    let interface_keys = [
        "address",
        "dns",
        "mtu",
        "privatekey",
        "listenport",
        "fwmark",
        "s1",
        "s2",
        "s3",
        "s4",
        "jc",
        "jmin",
        "jmax",
        "h1",
        "h2",
        "h3",
        "h4",
        "i1",
        "i2",
        "i3",
        "i4",
        "i5",
        "headerprotectionkey",
        "contentpaddingaddition",
        "rekeyaftertime",
        "rekeytimeout",
        "rejectaftertime",
        "keepalivetimeout",
        "maxhandshakeattempts",
        "randomtrailers",
        "disablecookies",
    ];
    let peer_keys = [
        "allowedips",
        "endpoint",
        "persistentkeepalive",
        "publickey",
        "presharedkey",
        "jc",
        "jmin",
        "jmax",
        "s1",
        "s2",
        "s3",
        "s4",
        "h1",
        "h2",
        "h3",
        "h4",
        "i1",
        "i2",
        "i3",
        "i4",
        "i5",
        "headerprotectionkey",
        "contentpaddingaddition",
        "rekeyaftertime",
        "rekeytimeout",
        "rejectaftertime",
        "keepalivetimeout",
        "maxhandshakeattempts",
        "randomtrailers",
        "disablecookies",
    ];
    let mut section = String::new();
    let mut interface_seen = false;
    let mut peer_seen = false;
    let mut interface_keys_seen = Vec::new();
    let mut peer_keys_seen = Vec::new();
    for raw_line in config.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].to_string();
            if section.eq_ignore_ascii_case("interface") {
                interface_seen = true;
            } else if section.eq_ignore_ascii_case("peer") {
                peer_seen = true;
            } else {
                return Err("В конфигурации AmneziaWG найдена неизвестная секция".to_string());
            }
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| "В конфигурации AmneziaWG есть строка неверного формата".to_string())?;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
            return Err(
                "В конфигурации AmneziaWG есть пустое или недопустимое значение".to_string(),
            );
        }
        if section.eq_ignore_ascii_case("interface") {
            if !interface_keys.contains(&key.as_str()) {
                return Err(format!("Параметр {key} в Interface запрещён"));
            }
            interface_keys_seen.push(key);
        } else if section.eq_ignore_ascii_case("peer") {
            if !peer_keys.contains(&key.as_str()) {
                return Err(format!("Параметр {key} в Peer запрещён"));
            }
            peer_keys_seen.push(key);
        } else {
            return Err("Параметр найден вне секции Interface или Peer".to_string());
        }
    }
    let has_required = interface_seen
        && peer_seen
        && interface_keys_seen.iter().any(|key| key == "privatekey")
        && interface_keys_seen.iter().any(|key| key == "address")
        && peer_keys_seen.iter().any(|key| key == "publickey")
        && peer_keys_seen.iter().any(|key| key == "endpoint")
        && peer_keys_seen.iter().any(|key| key == "allowedips");
    if !has_required {
        return Err("В конфигурации AmneziaWG не хватает ключей или адреса сервера".to_string());
    }
    Ok(())
}

fn awg_endpoint(config: &str) -> String {
    let mut in_peer = false;
    for line in config.lines() {
        let line = line.trim();
        if line.eq_ignore_ascii_case("[peer]") {
            in_peer = true;
            continue;
        }
        if line.starts_with('[') {
            in_peer = false;
        }
        if in_peer {
            if let Some((key, endpoint)) = line.split_once('=') {
                if key.trim().eq_ignore_ascii_case("endpoint") {
                    if let Some(value) = endpoint.trim().split('#').next() {
                        return value.trim().to_string();
                    }
                }
            }
        }
    }
    "Адрес не указан".to_string()
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let value = value.trim();
    [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ]
    .iter()
    .find_map(|engine| engine.decode(value).ok())
}

fn percent_decode(value: &str) -> String {
    let mut bytes = Vec::with_capacity(value.len());
    let source = value.as_bytes();
    let mut cursor = 0;
    while cursor < source.len() {
        if source[cursor] == b'%' && cursor + 2 < source.len() {
            if let Ok(hex) = u8::from_str_radix(&value[cursor + 1..cursor + 3], 16) {
                bytes.push(hex);
                cursor += 3;
                continue;
            }
        }
        bytes.push(source[cursor]);
        cursor += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_AWG_CONFIG: &str = "[Interface]\nPrivateKey = dummy-private-key\nAddress = 10.7.0.2/32\nDNS = 1.1.1.1\nMTU = 1280\nS1 = 10\nJc = 4\nH1 = 123456\nI1 = <b 0x01>\n[Peer]\nPublicKey = dummy-public-key\nPresharedKey = dummy-preshared-key\nEndpoint = vpn.example.test:443\nAllowedIPs = 0.0.0.0/0, ::/0\nPersistentKeepalive = 25\n";

    #[test]
    fn imports_amneziawg_json_subscription() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(TEST_AWG_CONFIG);
        let input = serde_json::json!([{
            "type": "amneziawg",
            "version": 1,
            "servers": [{"name": "Test node", "config": encoded}]
        }]);
        let profiles = parse_subscription(&input.to_string()).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].summary.protocol, "AmneziaWG");
        assert_eq!(profiles[0].summary.endpoint, "vpn.example.test:443");
        assert!(profiles[0].secret.contains("dummy-private-key"));
    }

    #[test]
    fn rejects_shell_hooks_in_awg_config() {
        let unsafe_config = TEST_AWG_CONFIG.replace(
            "MTU = 1280\n",
            "MTU = 1280\nPostUp = touch /tmp/commonnet-should-never-exist\n",
        );
        assert!(validate_awg_config(&unsafe_config).is_err());
    }

    #[test]
    fn builds_tun_config_for_vless_reality() {
        let outbound = build_xray_outbound(
            "vless://a9e4c1f0-6f15-4a51-8d31-000000000000@edge.example.test:443?encryption=none&security=reality&sni=example.org&fp=chrome&pbk=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA%3D&sid=1234&type=grpc&serviceName=main#test",
            "VLESS",
        )
        .unwrap();
        assert_eq!(outbound["protocol"], "vless");
        assert_eq!(outbound["streamSettings"]["security"], "reality");
        assert_eq!(
            outbound["streamSettings"]["grpcSettings"]["serviceName"],
            "main"
        );
    }

    #[test]
    fn rejects_unknown_xray_transport() {
        let result = build_xray_outbound(
            "vless://a9e4c1f0-6f15-4a51-8d31-000000000000@edge.example.test:443?security=tls&type=unknown",
            "VLESS",
        );
        assert!(result.is_err());
    }

    #[test]
    fn parses_incy_routing_profile_and_builds_xray_rules() {
        let payload = serde_json::json!({
            "Name": "Test routing",
            "GlobalProxy": "true",
            "RemoteDns": "8.8.8.8",
            "DomesticDns": "77.88.8.8",
            "RemoteDnsType": "DoH",
            "DomesticDnsType": "DoH",
            "RemoteDnsDomain": "https://8.8.8.8/dns-query",
            "DomesticDnsDomain": "https://77.88.8.8/dns-query",
            "DnsHosts": {"dns.example.test": "213.24.64.175"},
            "RouteOrder": "block-proxy-direct",
            "DirectSites": ["geosite:private", "geosite:category-ru"],
            "DirectIp": ["geoip:private", "geoip:direct"],
            "ProxySites": ["geosite:youtube", "geosite:telegram"],
            "ProxyIp": [],
            "BlockSites": ["geosite:win-spy"],
            "BlockIp": [],
            "DomainStrategy": "IPIfNonMatch",
            "FakeDNS": "false",
            "Geoipurl": "",
            "Geositeurl": ""
        });
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload.to_string());
        let routing = parse_routing_uri(&format!("incy://routing/onadd/{encoded}")).unwrap();
        assert_eq!(routing.name, "Test routing");
        assert!(routing.global_proxy);
        assert_eq!(routing.route_order, "block-proxy-direct");

        let profile = make_profile(
            "test-node",
            "VLESS",
            "edge.example.test:443".to_string(),
            "vless://a9e4c1f0-6f15-4a51-8d31-000000000000@edge.example.test:443?security=tls&sni=example.test#test".to_string(),
        );
        let config = build_xray_config_with_routing(&profile, Some(&routing)).unwrap();
        assert_eq!(config["outbounds"][0]["tag"], "proxy");
        assert_eq!(config["outbounds"][1]["tag"], "direct");
        assert_eq!(config["outbounds"][2]["tag"], "block");
        assert_eq!(config["routing"]["domainStrategy"], "IPIfNonMatch");
        assert_eq!(config["routing"]["rules"][0]["outboundTag"], "block");
        assert_eq!(config["routing"]["rules"][1]["outboundTag"], "proxy");
        assert_eq!(config["routing"]["rules"][2]["outboundTag"], "direct");
        assert_eq!(config["dns"]["hosts"]["dns.example.test"], "213.24.64.175");
        assert_eq!(config["inbounds"][0]["sniffing"]["enabled"], true);
    }

    #[test]
    fn rejects_malformed_incy_routing_links() {
        assert!(parse_routing_uri("https://example.test/routing").is_err());
        let encoded = base64::engine::general_purpose::STANDARD.encode("not-json");
        assert!(parse_routing_uri(&format!("incy://routing/onadd/{encoded}")).is_err());
    }

    #[test]
    #[ignore = "set COMMONNET_XRAY_BINARY and COMMONNET_XRAY_ASSET_DIR to check routing with Xray"]
    fn xray_core_accepts_incy_routing_configuration() {
        let binary = std::env::var("COMMONNET_XRAY_BINARY").unwrap();
        let assets = std::env::var("COMMONNET_XRAY_ASSET_DIR").unwrap();
        let routing = RoutingSettings {
            name: "test-routing".to_string(),
            global_proxy: true,
            remote_dns: "8.8.8.8".to_string(),
            domestic_dns: "77.88.8.8".to_string(),
            remote_dns_type: "DoH".to_string(),
            domestic_dns_type: "DoH".to_string(),
            remote_dns_domain: "https://8.8.8.8/dns-query".to_string(),
            domestic_dns_domain: "https://77.88.8.8/dns-query".to_string(),
            dns_hosts: HashMap::from([
                ("lkfl2.nalog.ru".to_string(), "213.24.64.175".to_string()),
                ("lknpd.nalog.ru".to_string(), "213.24.64.181".to_string()),
            ]),
            route_order: "block-proxy-direct".to_string(),
            direct_sites: [
                "private",
                "category-ru",
                "whitelist",
                "microsoft",
                "apple",
                "epicgames",
                "riot",
                "escapefromtarkov",
                "steam",
                "twitch",
                "pinterest",
                "faceit",
            ]
            .into_iter()
            .map(|tag| format!("geosite:{tag}"))
            .collect(),
            direct_ip: vec!["geoip:private".to_string(), "geoip:direct".to_string()],
            proxy_sites: ["google-play", "github", "twitch-ads", "youtube", "telegram"]
                .into_iter()
                .map(|tag| format!("geosite:{tag}"))
                .collect(),
            proxy_ip: Vec::new(),
            block_sites: ["win-spy", "torrent", "category-ads"]
                .into_iter()
                .map(|tag| format!("geosite:{tag}"))
                .collect(),
            block_ip: Vec::new(),
            domain_strategy: "IPIfNonMatch".to_string(),
            geoip_url: String::new(),
            geosite_url: String::new(),
            asset_version: "test".to_string(),
        };
        let profile = make_profile(
            "test-node",
            "VLESS",
            "edge.example.test:443".to_string(),
            "vless://a9e4c1f0-6f15-4a51-8d31-000000000000@edge.example.test:443?security=tls&sni=example.test#test".to_string(),
        );
        let mut config = build_xray_config_with_routing(&profile, Some(&routing)).unwrap();
        config["inbounds"] = serde_json::json!([]);
        let config_path = std::env::temp_dir().join("commonnet-xray-routing-check.json");
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        let result = Command::new(binary)
            .args(["run", "-test", "-config"])
            .arg(&config_path)
            .env("XRAY_LOCATION_ASSET", assets)
            .output()
            .unwrap();
        let _ = fs::remove_file(config_path);
        assert!(
            result.status.success(),
            "Xray rejected routing config: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    #[ignore = "set COMMONNET_DEBUG_SUBSCRIPTION to the local response body to check the supplied test subscription"]
    fn imports_supplied_debug_subscription() {
        let path = std::env::var("COMMONNET_DEBUG_SUBSCRIPTION").unwrap();
        let body = fs::read_to_string(path).unwrap();
        let profiles = parse_subscription(&body).unwrap();
        assert_eq!(profiles.len(), 6);
        assert!(
            profiles
                .iter()
                .all(|profile| profile.summary.protocol == "AmneziaWG")
        );
    }

    #[test]
    #[ignore = "set COMMONNET_XRAY_BINARY to an Xray executable to check generated TUN JSON"]
    fn xray_core_accepts_generated_tun_configuration() {
        let binary = std::env::var("COMMONNET_XRAY_BINARY").unwrap();
        let generated_keys = Command::new(&binary).arg("x25519").output().unwrap();
        assert!(generated_keys.status.success());
        let reality_password = String::from_utf8_lossy(&generated_keys.stdout)
            .lines()
            .find_map(|line| line.strip_prefix("Password (PublicKey): "))
            .unwrap()
            .trim()
            .to_string();
        let vmess_json = serde_json::json!({
            "add": "edge.example.test", "port": "443", "id": "a9e4c1f0-6f15-4a51-8d31-000000000000",
            "aid": "0", "scy": "auto", "net": "ws", "tls": "tls", "host": "example.org", "path": "/socket"
        });
        let vmess = format!(
            "vmess://{}",
            base64::engine::general_purpose::STANDARD.encode(vmess_json.to_string())
        );
        let profiles = [
            make_profile(
                "test-vless",
                "VLESS",
                "edge.example.test:443".to_string(),
                format!(
                    "vless://a9e4c1f0-6f15-4a51-8d31-000000000000@edge.example.test:443?encryption=none&security=reality&sni=example.org&fp=chrome&pbk={reality_password}&sid=1234&type=tcp#test"
                ),
            ),
            make_profile(
                "test-vmess",
                "VMess",
                "edge.example.test:443".to_string(),
                vmess,
            ),
            make_profile(
                "test-trojan",
                "Trojan",
                "edge.example.test:443".to_string(),
                "trojan://secret@edge.example.test:443?security=tls&sni=example.org#test"
                    .to_string(),
            ),
            make_profile(
                "test-ss",
                "Shadowsocks",
                "edge.example.test:8388".to_string(),
                "ss://aes-128-gcm:secret@edge.example.test:8388#test".to_string(),
            ),
            make_profile(
                "test-socks",
                "SOCKS",
                "edge.example.test:1080".to_string(),
                "socks5://user:secret@edge.example.test:1080#test".to_string(),
            ),
        ];
        let config_path = std::env::temp_dir().join("commonnet-xray-config-check.json");
        for profile in profiles {
            let mut config = build_xray_config(&profile).unwrap();
            assert_eq!(config["inbounds"][0]["protocol"], "tun");
            assert_eq!(
                config["inbounds"][0]["settings"]["autoSystemRoutingTable"][0],
                "0.0.0.0/0"
            );
            // `-test` validates outbound and transport schemas without opening a privileged TUN device.
            config["inbounds"] = serde_json::json!([]);
            fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
            let result = Command::new(&binary)
                .args(["run", "-test", "-config"])
                .arg(&config_path)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "Xray rejected {} configuration ({}): {} {}",
                profile.summary.protocol,
                result.status,
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr),
            );
        }
        let _ = fs::remove_file(config_path);
    }
}

pub fn run() {
    let app = tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            import_subscription,
            saved_subscription_url,
            import_routing_profile,
            saved_routing_profile,
            list_profiles,
            runtime_status,
            connection_status,
            connect_profile,
            disconnect_profile
        ])
        .build(tauri::generate_context!())
        .expect("ошибка запуска Commonnet Desktop");
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            let state = handle.state::<AppState>();
            if let Ok(mut active_guard) = state.active.lock() {
                if let Some(mut active) = active_guard.take() {
                    let _ = stop_platform_tunnel(&handle, &mut active);
                    let _ = fs::remove_file(active.config_path);
                }
            }
        }
    });
}
