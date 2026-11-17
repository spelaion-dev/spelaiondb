// Settings → how the grid shows values (SDB-385): date, time and number formats, the display
// time zone and how binary reads. Display only: the core formats each window it serves and
// keeps the database's own text for copies, exports, filters and edits. The core checks every
// setting before saving it; what it refuses is shown here and nothing changes.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import { useDisplayFormat, type DisplayFormat } from "../displayFormat";

/** The presets of each kind: `strftime` patterns the core formats with. The first, empty,
 *  is "as the database prints it". */
const DATE_PRESETS = ["", "%d/%m/%Y", "%m/%d/%Y", "%d.%m.%Y", "%d %b %Y"];
const TIME_PRESETS = ["", "%H:%M:%S", "%H:%M", "%I:%M:%S %p"];
const TIMESTAMP_PRESETS = ["", "%d/%m/%Y %H:%M:%S", "%m/%d/%Y %I:%M:%S %p", "%d.%m.%Y %H:%M:%S", "%d %b %Y %H:%M:%S", "%Y-%m-%d %H:%M"];
/** What each preset looks like, for 16 November 2026, 20:07:16. */
const SAMPLES: Record<string, string> = {
  "%d/%m/%Y": "16/11/2026",
  "%m/%d/%Y": "11/16/2026",
  "%d.%m.%Y": "16.11.2026",
  "%d %b %Y": "16 Nov 2026",
  "%H:%M:%S": "20:07:16",
  "%H:%M": "20:07",
  "%I:%M:%S %p": "08:07:16 PM",
  "%d/%m/%Y %H:%M:%S": "16/11/2026 20:07:16",
  "%m/%d/%Y %I:%M:%S %p": "11/16/2026 08:07:16 PM",
  "%d.%m.%Y %H:%M:%S": "16.11.2026 20:07:16",
  "%d %b %Y %H:%M:%S": "16 Nov 2026 20:07:16",
  "%Y-%m-%d %H:%M": "2026-11-16 20:07",
};
const NUMBER_PRESETS: { id: string; grouping: string; decimal: string; sample: string }[] = [
  { id: "stored", grouping: "none", decimal: "dot", sample: "1234567.89" },
  { id: "comma-dot", grouping: "comma", decimal: "dot", sample: "1,234,567.89" },
  { id: "dot-comma", grouping: "dot", decimal: "comma", sample: "1.234.567,89" },
  { id: "space-comma", grouping: "space", decimal: "comma", sample: "1 234 567,89" },
  { id: "apostrophe-dot", grouping: "apostrophe", decimal: "dot", sample: "1'234'567.89" },
  { id: "none-comma", grouping: "none", decimal: "comma", sample: "1234567,89" },
];
const CUSTOM = "custom";

function Pattern({ kind, presets, value, disabled, onChange }: {
  kind: "date" | "time" | "timestamp";
  presets: string[];
  value: string;
  disabled: boolean;
  onChange(pattern: string): Promise<boolean>;
}) {
  const { t } = useTranslation("settings");
  const [custom, setCustom] = useState(!presets.includes(value));
  const [typed, setTyped] = useState(value);
  useEffect(() => { setTyped(value); if (!presets.includes(value)) setCustom(true); }, [value, presets]);
  const save = () => { if (typed !== value) void onChange(typed).then((ok) => { if (!ok) setTyped(value); }); };
  return (
    <div className="display-format-row">
      <label className="field">
        <span>{t(`display.${kind}`)}</span>
        <select
          name={`display-${kind}`}
          value={custom ? CUSTOM : value}
          disabled={disabled}
          onChange={(e) => {
            if (e.target.value === CUSTOM) { setCustom(true); return; }
            setCustom(false);
            void onChange(e.target.value);
          }}
        >
          {presets.map((pattern) => (
            <option key={pattern} value={pattern}>{pattern === "" ? t("display.asDatabase") : SAMPLES[pattern] ?? pattern}</option>
          ))}
          <option value={CUSTOM}>{t("display.custom")}</option>
        </select>
      </label>
      {custom && (
        <label className="field">
          <span>{t("display.pattern")}</span>
          <input
            className="mono"
            name={`display-${kind}-pattern`}
            value={typed}
            disabled={disabled}
            maxLength={64}
            spellCheck={false}
            autoComplete="off"
            onChange={(e) => setTyped(e.target.value)}
            onBlur={save}
            onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); save(); } }}
          />
        </label>
      )}
    </div>
  );
}

export function DisplaySettings() {
  const { t } = useTranslation("settings");
  const format = useDisplayFormat((s) => s.format);
  const loaded = useDisplayFormat((s) => s.loaded);
  const error = useDisplayFormat((s) => s.error);
  const save = useDisplayFormat((s) => s.save);
  const [zones, setZones] = useState<string[]>([]);
  const [zone, setZone] = useState(format.timeZone);
  useEffect(() => { void useDisplayFormat.getState().load(); }, []);
  useEffect(() => setZone(format.timeZone), [format.timeZone]);
  useEffect(() => {
    call(commands.timeZones()).then(setZones).catch(() => setZones([]));
  }, []);
  const change = (patch: Partial<DisplayFormat>) => save({ ...format, ...patch });
  const off = !loaded || format.asStored;
  const number = NUMBER_PRESETS.find((preset) => preset.grouping === format.grouping && preset.decimal === format.decimal)?.id ?? "stored";
  const saveZone = () => { if (zone.trim() !== format.timeZone) void change({ timeZone: zone.trim() }).then((ok) => { if (!ok) setZone(format.timeZone); }); };
  return (
    <>
      <fieldset className="settings-choice display-format" disabled={!loaded}>
        <legend>{t("display.title")}</legend>
        <label className="check">
          <input type="checkbox" name="displayAsStored" checked={format.asStored} onChange={(e) => void change({ asStored: e.target.checked })} />
          {t("display.asStored")}
        </label>
        <Pattern kind="date" presets={DATE_PRESETS} value={format.date} disabled={off} onChange={(date) => change({ date })} />
        <Pattern kind="time" presets={TIME_PRESETS} value={format.time} disabled={off} onChange={(time) => change({ time })} />
        <Pattern kind="timestamp" presets={TIMESTAMP_PRESETS} value={format.timestamp} disabled={off} onChange={(timestamp) => change({ timestamp })} />
        <div className="display-format-row">
          <label className="field">
            <span>{t("display.numbers")}</span>
            <select
              name="display-numbers"
              value={number}
              disabled={off}
              onChange={(e) => {
                const preset = NUMBER_PRESETS.find((item) => item.id === e.target.value);
                if (preset) void change({ grouping: preset.grouping, decimal: preset.decimal });
              }}
            >
              {NUMBER_PRESETS.map((preset) => (
                <option key={preset.id} value={preset.id}>{preset.id === "stored" ? t("display.asDatabase") : preset.sample}</option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>{t("display.binary")}</span>
            <select name="display-binary" value={format.binary} disabled={off} onChange={(e) => void change({ binary: e.target.value })}>
              <option value="hex">{t("display.binaryHex")}</option>
              <option value="utf8">{t("display.binaryUtf8")}</option>
              <option value="latin1">{t("display.binaryLatin1")}</option>
              <option value="base64">{t("display.binaryBase64")}</option>
            </select>
          </label>
        </div>
        <div className="display-format-row">
          <label className="field">
            <span>{t("display.timeZone")}</span>
            <input
              className="mono"
              name="display-time-zone"
              list="display-time-zones"
              value={zone}
              disabled={off}
              placeholder={t("display.timeZonePlaceholder")}
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setZone(e.target.value)}
              onBlur={saveZone}
              onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); saveZone(); } }}
            />
            <datalist id="display-time-zones">
              <option value="local" />
              {zones.map((name) => <option key={name} value={name} />)}
            </datalist>
          </label>
        </div>
      </fieldset>
      <p className="muted small">{t("display.hint")}</p>
      <p className="muted small">{t("display.patternHint")}</p>
      {error && <p className="error small" role="alert">{t("display.refused", { detail: error })}</p>}
    </>
  );
}
