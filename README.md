# Commonnet Desktop

Настольный VPN-клиент Commonnet для Windows, macOS и Linux на Tauri 2. Интерфейс импортирует подписку, показывает список серверов и запускает системный туннель через Xray или AmneziaWG.

## Возможности

- Подписки по HTTPS: JSON AmneziaWG с Base64-конфигурациями и текстовые ссылки VLESS, VMess, Trojan, Shadowsocks и SOCKS.
- Импорт профиля маршрутизации Incy в настройках: правила Direct/Proxy/Block, DNS, DNS-хосты и пользовательские `geoip.dat`/`geosite.dat`. Профиль сохраняется в данных приложения и применяется к Xray; маршрутизация Incy не поддерживается профилями AmneziaWG.
- Полный туннель IPv4/IPv6 через Xray TUN. На Windows включена блокировка DNS и трафика, который не попал в TUN.
- AmneziaWG на Linux и macOS через официальный userspace core; на Windows через официальный клиент и Wintun.
- Права запрашиваются системным диалогом: Polkit, macOS administrator dialog или Windows UAC.
- На Linux Polkit запрашивает пароль при первом подключении за запуск клиента; системный помощник остаётся активен до закрытия приложения, поэтому повторные подключения и отключения не требуют нового ввода пароля.
- Движки собираются и включаются в установщик; Xray архив проверяется по SHA-256 до распаковки.
- Ссылка подписки хранится в папке данных приложения (на Unix с правами `0600`), загружается при запуске и обновляется каждый час, пока клиент открыт. Названия и адреса серверов кэшируются локально, чтобы список оставался виден после перезапуска; секреты профилей остаются в памяти. Пока туннель включён, временная конфигурация хранится в папке приложения с ограниченными правами. На Linux системный помощник копирует её в защищённую папку `/run` и удаляет при отключении или штатном закрытии клиента.
- Конфигурация AmneziaWG проверяется по разрешённым ключам; команды из `PostUp`/`PostDown` и неизвестные параметры отклоняются.
- Обновление приложения выполняется вручную установкой нового пакета из GitHub Releases. Подписка и настройки маршрутизации хранятся в каталоге данных приложения и сохраняются при установке новой версии поверх старой.

## Сборка

Для локальной сборки нужны Node.js 20+, Rust stable, Git и Go 1.25+.

На Linux дополнительно требуются системные пакеты Tauri 2, C-компилятор и `make`. На macOS нужны Xcode Command Line Tools. На Windows сборщик AmneziaWG использует официальный `build.bat` и скачивает закреплённые зависимости с проверкой хешей.

```sh
npm install
npm run engines:all
npm run tauri dev
```

Собрать установщики текущей ОС:

```sh
npm run tauri build
```

### GitHub Releases

Публикация тега `v*` запускает `.github/workflows/release.yml`. GitHub Actions собирает Linux x64 и ARM64 (`.deb`, `.rpm`, AppImage), Windows x64 и ARM64 (`.msi`, `.exe`), macOS Intel и Apple Silicon (`.dmg`), затем прикрепляет файлы к GitHub Release. Для выпуска новой версии синхронно обнови версию в `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock` и `src-tauri/tauri.conf.json`, создай соответствующий тег и отправь его:

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

Установщики Windows и macOS собираются без подписи, если в репозитории не настроены сертификаты подписи.

Скрипты закрепляют Xray `v26.9.30`, AmneziaWG tools `v3.1.20260812`, AmneziaWG Go `v3.1.20260828` и официальный Windows-клиент `3.1.0`. На macOS также собирается GNU Bash 5.3: системный Bash старше не становится, а `awg-quick` требует Bash 4 или новее.

## Проверка

В Rust есть проверки импорта подписки и профилей, а также матрица проверки Xray-ссылок реальным Xray core. Локальный ответ подписки можно проверить без вывода конфигураций в терминал:

```sh
COMMONNET_DEBUG_SUBSCRIPTION=/path/to/subscription.json \
COMMONNET_XRAY_BINARY=src-tauri/resources/engines/xray \
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --include-ignored
```

Проверка реального сетевого соединения требует запускать установщик на каждой поддерживаемой ОС: туннель создаёт системный интерфейс и изменяет маршруты. GitHub Actions собирает пакеты на нативных раннерах. Публичное распространение macOS требует подписи и notarization; Windows-релиз следует подписать сертификатом издателя.

## Сторонние компоненты

Движки Xray и AmneziaWG распространяются со своими лицензиями. Файлы лицензий включаются в сборку вместе с движками. Исходники и инструкции по сборке:

- Xray core: <https://github.com/XTLS/Xray-core>
- AmneziaWG tools: <https://github.com/amnezia-vpn/amneziawg-tools>
- AmneziaWG Go: <https://github.com/amnezia-vpn/amneziawg-go>
- AmneziaWG Windows client: <https://github.com/amnezia-vpn/amneziawg-windows-client>
- GNU Bash: <https://www.gnu.org/software/bash/>
