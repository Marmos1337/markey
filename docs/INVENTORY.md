# Инвентаризация Rekey — 09.10.2026

Поиск выполнен по домашней папке пользователя, /Applications и /Volumes; ошибки доступа при первоначальном поиске не считаются доказательством отсутствия файлов. Совпадения `prekey` в MarMic не относятся к Rekey.

| Найдено | Расположение | Сохранение |
|---|---|---|
| Рабочие исходники | `/Users/stepan/Documents/Codex/2026-10-01/task-3/rekey-source` | В корне MarKey; 78 исходных файлов, кроме README, который сохранён в docs/REKEY-UPSTREAM.md |
| Rescue source, bundle, patch, manifest, отчёты | `task-3/cloud-export-20261002/rekey-rescue` | Source совпадает с рабочими исходниками; остальные материалы в archive/rekey-2026-10-02 |
| Полный rescue tar.gz | `task-3/cloud-export-20261002/Rekey-paused-source-history-evidence-20261002.tar.gz` | Оставлен локально как дублирующий архив; hash в inventory.json |
| Отчёты и снимки настроек | `task-3/evidence` | Текстовые отчёты сохранены в archive; изображения оставлены локально |
| Установленное приложение | `/Users/stepan/Applications/Rekey.app` | Найдено, оставлено локально |
| Предыдущие приложения | `task-3/backups/Rekey-V3.app`, `Rekey-V4-r4.app` | Найдены, оставлены локально |
| Настройки и runtime log | `~/Library/Application Support/app.rekey.desktop` | Оставлены локально; не являются исходным кодом |
| WebKit data | `~/Library/WebKit/app.rekey.desktop` | Оставлены локально |
| Сборки, инструменты, зависимости, модели и корпуса | `task-3/rekey-source/target`, `task-3/toolchain`, `task-3/downloads`, ignored caches | Не загружены: производные файлы и зависимости |

[Машиночитаемый список](inventory.json) содержит пути, размеры и SHA-256 файлов проекта и приложений; для личных runtime-данных только пути и размеры. Исходные папки и установленное приложение не изменены.

Перенос проверяется побайтовым сравнением с rescue snapshot, `git fsck`, проверкой bundle, приватности GitHub и равенства локального/удалённого HEAD. Полная сборка и runtime-тесты в эту задачу не входят.

## Проверено при переносе

78/78 исходных файлов побайтово совпадают с rescue export (исходный README в docs/REKEY-UPSTREAM.md). Git bundle verify и git fsck --full прошли. Проверка распространённых форматов секретов в файлах и 270 blob всей восстановленной истории не нашла совпадений; это не полный аудит безопасности. GitHub Actions отключены на уровне нового репозитория, чтобы унаследованные schedules/publishing не запускались.
