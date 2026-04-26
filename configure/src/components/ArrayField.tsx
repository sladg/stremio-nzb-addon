import { useState } from "react";
import {
  Control,
  FieldErrors,
  FieldValues,
  Path,
  UseFormGetValues,
  useFieldArray,
} from "react-hook-form";
import type { HealthcheckResult } from "../schemas";
import { validateIndexer, validateNntp } from "../schemas";
import PasswordField from "./PasswordField";
import ValidationStatus from "./ValidationStatus";

type ValidationState = "idle" | "validating" | "valid" | "invalid";
type RowValidation = { state: ValidationState; error?: string };

interface ArrayFieldProps<T extends FieldValues> {
  name: Path<T>;
  control: Control<T>;
  errors: FieldErrors<T>;
  register: any;
  getValues: UseFormGetValues<T>;
  title: string;
  required?: boolean;
  type: "indexers" | "nntpServers";
}

/**
 * Array of indexer or NNTP-server rows with onBlur-driven async validation.
 *
 * Validation strategy: when a field loses focus we read the current row
 * values via react-hook-form's getValues() and fire the appropriate
 * healthcheck endpoint. No useEffect, no debouncing, no watching - the
 * blur event already gives us a natural "user is done with this field"
 * signal.
 */
function ArrayField<T extends FieldValues>({
  name,
  control,
  errors,
  register,
  getValues,
  title,
  required,
  type,
}: ArrayFieldProps<T>) {
  const { fields, append, remove } = useFieldArray({
    control,
    name: name as any,
  });

  const [validation, setValidation] = useState<Record<string, RowValidation>>(
    {},
  );

  const runRowCheck = async (fieldId: string, index: number) => {
    const row = (getValues(name) as any[])?.[index];
    if (!row) return;

    const ready =
      type === "indexers" ? row.url && row.apiKey : row.server;
    if (!ready) {
      setValidation((prev) => ({ ...prev, [fieldId]: { state: "idle" } }));
      return;
    }

    setValidation((prev) => ({ ...prev, [fieldId]: { state: "validating" } }));

    const result: HealthcheckResult =
      type === "indexers"
        ? await validateIndexer(row.url, row.apiKey)
        : await validateNntp(row.server);

    setValidation((prev) => ({
      ...prev,
      [fieldId]: {
        state: result.ok ? "valid" : "invalid",
        error: result.error,
      },
    }));
  };

  // Reset to idle as soon as the user starts editing again
  const markDirty = (fieldId: string) => {
    setValidation((prev) =>
      prev[fieldId]?.state === "idle"
        ? prev
        : { ...prev, [fieldId]: { state: "idle" } },
    );
  };

  const fieldErrors = (errors as any)[name] as any[] | undefined;

  return (
    <div className="form-element">
      <div className="label-to-top">
        {title}
        {required && <span style={{ color: "red" }}> *</span>}
      </div>

      <div style={{ marginTop: 4, marginBottom: 8 }}>
        <button
          type="button"
          className="add-array-row"
          onClick={() =>
            append(
              (type === "indexers"
                ? { url: "", apiKey: "" }
                : { server: "" }) as any,
            )
          }
        >
          + Add {title}
        </button>
      </div>

      {fields.map((field, index) => {
        const rowErrors = fieldErrors?.[index];
        const rowValidation = validation[field.id] || { state: "idle" };

        const onBlur = () => runRowCheck(field.id, index);
        const onInput = () => markDirty(field.id);

        return (
          <div key={field.id} className="array-row">
            {type === "indexers" ? (
              <>
                <div className="array-row-field">
                  <input
                    type="text"
                    {...register(`${name}.${index}.url`)}
                    placeholder="Indexer URL (e.g., https://api.nzbgeek.info)"
                    className="full-width"
                    onBlur={onBlur}
                    onInput={onInput}
                  />
                </div>
                {rowErrors?.url && (
                  <span className="validation-error">
                    {rowErrors.url.message}
                  </span>
                )}
                <div className="array-row-field">
                  <PasswordField
                    {...register(`${name}.${index}.apiKey`)}
                    placeholder="API Key"
                    onBlur={onBlur}
                    onInput={onInput}
                  />
                </div>
                {rowErrors?.apiKey && (
                  <span className="validation-error">
                    {rowErrors.apiKey.message}
                  </span>
                )}
              </>
            ) : (
              <>
                <div className="array-row-field">
                  <PasswordField
                    {...register(`${name}.${index}.server`)}
                    placeholder="nntps://user:pass@news.example.com:563/8"
                    onBlur={onBlur}
                    onInput={onInput}
                  />
                </div>
                {rowErrors?.server && (
                  <span className="validation-error">
                    {rowErrors.server.message}
                  </span>
                )}
              </>
            )}

            <ValidationStatus
              state={rowValidation.state}
              error={rowValidation.error}
            />

            <div className="array-row-actions">
              {fields.length > 1 && (
                <button
                  type="button"
                  className="remove-array-row"
                  onClick={() => remove(index)}
                >
                  Remove
                </button>
              )}
            </div>
          </div>
        );
      })}

      {(errors as any)[name]?.root && (
        <span className="validation-error">
          {(errors as any)[name].root.message}
        </span>
      )}
    </div>
  );
}

export default ArrayField;
