type ValidationState = "idle" | "validating" | "valid" | "invalid";

interface ValidationStatusProps {
  state: ValidationState;
  error?: string;
}

/**
 * Displays validation status indicator (spinner, checkmark, or X)
 * with optional error message
 */
const ValidationStatus = ({ state, error }: ValidationStatusProps) => {
  if (state === "idle") return null;

  return (
    <div className="array-row-validation">
      <span className={`validation-status ${state}`}>
        {state === "valid" && "\u2705"}
        {state === "invalid" && "\u274C"}
      </span>
      {state === "invalid" && error && (
        <span className="validation-error">{error}</span>
      )}
    </div>
  );
};

export default ValidationStatus;
