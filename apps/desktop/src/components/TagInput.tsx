// Tags typed as chips: Enter, a comma or leaving the box adds what was typed, Backspace in
// an empty box removes the last one. The core normalises and validates them again.

import { useId, useState } from "react";
import { useTranslation } from "react-i18next";
import { MAX_TAG_LENGTH, MAX_TAGS, addTags, sameTag } from "../connectionInfo";

export function TagInput({
  label,
  tags,
  suggestions,
  onChange,
}: {
  label: string;
  tags: string[];
  /** Tags already used by other connections, offered while typing. */
  suggestions: string[];
  onChange: (tags: string[]) => void;
}) {
  const { t } = useTranslation("connections");
  const [text, setText] = useState("");
  const listId = useId();
  const full = tags.length >= MAX_TAGS;

  function commit(value: string) {
    const next = addTags(tags, value.split(","));
    if (next.length !== tags.length) onChange(next);
    setText("");
  }

  return (
    <div className="tag-input">
      {tags.map((tag) => (
        <span key={tag} className="tag-chip">
          {tag}
          <button
            type="button"
            className="ghost"
            aria-label={t("fields.removeTag", { tag })}
            title={t("fields.removeTag", { tag })}
            onClick={() => onChange(tags.filter((other) => other !== tag))}
          >
            ×
          </button>
        </span>
      ))}
      <input
        type="text"
        aria-label={label}
        list={listId}
        value={text}
        maxLength={MAX_TAG_LENGTH + 1}
        disabled={full}
        placeholder={full ? t("fields.tagsFull", { count: MAX_TAGS }) : tags.length === 0 ? t("fields.tagsPlaceholder") : ""}
        onChange={(e) => {
          const value = e.target.value;
          if (value.includes(",")) commit(value);
          else setText(value);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            // Enter adds the tag; it never submits the form.
            e.preventDefault();
            if (text.trim()) commit(text);
          } else if (e.key === "Backspace" && text === "" && tags.length > 0) {
            onChange(tags.slice(0, -1));
          }
        }}
        onBlur={() => {
          if (text.trim()) commit(text);
        }}
      />
      <datalist id={listId}>
        {suggestions
          .filter((s) => !tags.some((tag) => sameTag(tag, s)))
          .map((s) => (
            <option key={s} value={s} />
          ))}
      </datalist>
    </div>
  );
}
