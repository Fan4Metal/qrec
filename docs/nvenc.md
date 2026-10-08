# NVENC и кодеры Media Foundation: справка

Рабочая заметка, 2026-10-08: окно уже переделано, пропорций области ещё нет.

## Как кодирует qrec сейчас

- Кодер — **NVIDIA H.264 Encoder MFT** (`nvEncMFTH264x.dll` из драйвера): обёртка Media Foundation над тем же NVENC. Если его нет, используется аппаратный кодер другого производителя или программный `H264 Encoder MFT` Microsoft (`src/venc.rs`).
- Качество — только целевой битрейт: `ширина × высота × кадров/с × бит на пиксель` (0,05 / 0,1 / 0,2), в пределах 0,5–60 Мбит/с (`recorder::bitrate`), передаётся как `MF_MT_AVG_BITRATE`.
- Не зависят от качества: профиль H.264 High, прогрессивная развёртка, опорный кадр каждые 2 с (`MF_MT_MAX_KEYFRAME_SPACING`), NV12 BT.709 (видеопроцессор D3D11).
- Режим управления битрейтом явно не задаётся. По умолчанию у кодера NVIDIA — CBR (значение 0), но без заполнения до битрейта: секунда почти неподвижного окна заняла около 39 КБ при расчётных ~130 КБ.
- B-кадров нет: запись 1 с — 1 I-кадр и 30 P-кадров, `has_b_frames=0` (ffprobe).

## Машина разработки

- RTX 4070 Ti SUPER, драйвер 610.74.
- Кодеры Media Foundation (категория `MFT_CATEGORY_VIDEO_ENCODER`, реестр `HKLM\SOFTWARE\Classes\MediaFoundation\Transforms\Categories\f79eac7d-e545-4387-bdee-d647d7bde42a`):
  - NVIDIA H.264 Encoder MFT `{60F44560-5A20-4857-BFEF-D29773CB8040}`
  - NVIDIA HEVC Encoder MFT `{966F107C-8EA2-425D-B822-E4A71BEF01D7}`
  - NVIDIA AV1 Encoder MFT `{80B80715-8C5A-420D-B346-1A9DC40A5880}`
  - H264 Encoder MFT (Microsoft, программный) `{6ca50344-051a-4ded-9779-a43305165e35}`
- `C:\Windows\System32\nvEncodeAPI64.dll` (версия файла 32.0.16.1074) — прямой API NVENC, ставится с драйвером.

## Что принимают кодеры через ICodecAPI

Проверка: `MFTEnumEx` по H.264 / HEVC / AV1, `ActivateObject`, `MF_TRANSFORM_ASYNC_UNLOCK`, приведение к `ICodecAPI`; `IsSupported` и `GetValue` по свойствам, `SetValue` для каждого режима `CODECAPI_AVEncCommonRateControlMode`; до и после `SetOutputType` (1920×1080, 30 кадров/с, 6 Мбит/с). Программа-проверка была временной (отдельный крейт; методам `ICodecAPI::GetValue/SetValue` в крейте `windows` нужна фича `Win32_System_Ole`, которой в qrec нет).

| Свойство | NVIDIA H.264 | NVIDIA HEVC | NVIDIA AV1 | Microsoft H.264 |
|---|---|---|---|---|
| Режимы битрейта (принимаются) | все 7: CBR, PeakConstrainedVBR, UnconstrainedVBR, Quality, LowDelayVBR, GlobalVBR, GlobalLowDelayVBR | все 7 | все 7 | CBR, PeakConstrainedVBR, UnconstrainedVBR, Quality |
| `AVEncCommonQuality` (по умолчанию) | да (65) | да (65) | да (65) | да (65) |
| `AVEncVideoEncodeQP` (по умолчанию) | да (26) | да (26) | да (30, диапазон 1–63) | да (26), но `SetValue` после типа — ошибка |
| `AVEncCommonQualityVsSpeed` | да (33) | да | да | да |
| Min/Max QP | 0–51 | 0–51 | 1–63 | 0–51 |
| `AVEncMPVDefaultBPictureCount` | **нет** | **нет** | **нет** | да (1) |
| `AVEncAdaptiveMode`, `AVEncVideoContentType` | нет | нет | нет | да |
| `AVLowLatencyMode` | да | да | да | да |
| После `SetOutputType` у NVIDIA | MeanBitRate 5 184 000 (при заданных 6 000 000), MaxBitRate 15 552 000, BufferSize 1 944 000, GOPSize 30 | то же | то же | MeanBitRate 6 000 000 |

Оговорка: принятое значение ещё не значит, что оно влияет на результат. Пробного кодирования в каждом режиме не было.

## Прямой NVENC (NVIDIA Video Codec SDK)

### Доступно и через Media Foundation

- Постоянное качество (Quality) или фиксированный QP вместо битрейта.
- HEVC и AV1 (на том же качестве файл заметно меньше, чем в H.264).
- Опорные кадры, пределы QP, низкая задержка.

### Только напрямую

- **B-кадры**: обычно −10–20 % размера на том же качестве; на экранном содержимом выигрыш скромнее.
- **Пресеты P1–P7 и настройка под задачу** (максимальное качество, низкая задержка, без потерь), **lookahead**, **адаптивное квантование** (spatial/temporal AQ). Польза AQ для записи экрана не очевидна — проверять.
- **4:4:4** (H.264 High 4:4:4, HEVC RExt): чёткий цветной мелкий текст (4:2:0 его размывает). Воспроизведение плохое: браузеры и большинство аппаратных декодеров H.264 4:4:4 не поддерживают.
- **Без потерь** (H.264/HEVC): для монтажа, файлы огромные.
- **10 бит HEVC/AV1 с метаданными HDR**: путь к записи HDR-экранов (сейчас qrec их отклоняет). Нужен ещё шейдер scRGB → PQ/BT.2020.
- **Вход прямо в BGRA** (`NV_ENC_BUFFER_FORMAT_ARGB`): NVENC сам переводит в YUV, видеопроцессор можно убрать. Какую матрицу он применяет — проверить.

### Цена

- Только NVIDIA: для Intel/AMD и программного кодера остаётся Media Foundation — две ветки кодирования. У Intel и AMD свои SDK (oneVPL/QSV, AMF), каждый — отдельная работа.
- Привязки к Rust писать самим. `nvEncodeAPI.h` под MIT (по памяти; проверить в SDK). Существующие крейты ориентированы на CUDA и Linux; для D3D11 на Windows скорее всего нужно собственное подмножество структур (порядка тысячи строк). У каждой структуры номер версии; ошибка даёт `NV_ENC_ERR_INVALID_VERSION`. Отлаживать можно на этой машине.
- Схема: `LoadLibrary("nvEncodeAPI64.dll")` → `NvEncodeAPIGetMaxSupportedVersion` → `NvEncodeAPICreateInstance` → `nvEncOpenEncodeSessionEx` с `NV_ENC_DEVICE_TYPE_DIRECTX` и `ID3D11Device` → `nvEncRegisterResource` для текстур (`NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX`) → `nvEncMapInputResource` / `nvEncEncodePicture` / `nvEncLockBitstream`. Готовые байты H.264 отдаются в `encoder.rs` как сейчас (sink writer принимает сжатый поток).
- Минимальная версия драйвера растёт с версией SDK (по памяти: 12.0 — R520+, 13.0 — R570+). Собирать под не самую новую версию API, проверять при запуске.
- В дистрибутив ничего не добавляется: DLL из драйвера.
- Лимит одновременных сессий NVENC на GeForce и ноутбуки с экраном на встроенной графике касаются и MFT — тут ничего не меняется.
- Срок: H.264 напрямую (асинхронная выдача, регистрация текстур D3D11, проверка) — несколько дней; HEVC и AV1 после этого почти бесплатно.

## HEVC и AV1 в MP4

- Приёмник MP4 Media Foundation умеет HEVC; AV1 в MP4 на Windows 10 — проверить.
- Воспроизведение: HEVC в стандартных проигрывателях Windows требует платного «HEVC Video Extensions»; Chrome/Edge играют HEVC при аппаратной поддержке. AV1 — бесплатный «AV1 Video Extension», в браузерах поддерживается хорошо. H.264 самый совместимый.

## План

1. **Через Media Foundation, без прямого NVENC** (для всех производителей, правки в основном в `src/venc.rs`):
   - пробное кодирование в режиме Quality и с фиксированным QP; сравнить с нынешним CBR по размеру и картинке на тексте, прокрутке и видео в кадре;
   - если режим работает — «Низкое / Среднее / Высокое» становятся уровнями качества (`AVEncCommonQuality`), а не битрейтом; оценку «до N МБ/мин» в окне пересмотреть (при постоянном качестве верхней границы нет);
   - отдельной настройкой — кодек HEVC или AV1, когда такой кодер найден; проверить запись в MP4 и воспроизведение.
2. **Прямой NVENC** отдельной веткой — только ради того, чего нет в Media Foundation: B-кадры, 4:4:4, без потерь, HDR.
