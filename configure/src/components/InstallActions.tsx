import { useState } from "react";

interface InstallActionsProps {
  installLink: string;
  isValid: boolean;
}

/**
 * Install + copy buttons.
 *
 * - Install: stremio:// deep-link, opens the desktop/mobile app.
 * - Copy: http(s):// version of the same manifest URL, suitable for
 *   pasting into Stremio Web's "Add addon" field or for inspection.
 *   Uses the current page protocol so it matches what's actually serving us.
 */
const InstallActions = ({ installLink, isValid }: InstallActionsProps) => {
  const [copied, setCopied] = useState(false);

  const httpLink = installLink.replace(
    /^stremio:\/\//,
    `${window.location.protocol}//`,
  );

  const onCopy = async () => {
    if (!isValid) return;
    try {
      await navigator.clipboard.writeText(httpLink);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // fall back to legacy execCommand for non-secure-context browsers
      const ta = document.createElement("textarea");
      ta.value = httpLink;
      document.body.appendChild(ta);
      ta.select();
      document.execCommand("copy");
      document.body.removeChild(ta);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    }
  };

  return (
    <div style={{ display: "flex", gap: 8, alignItems: "stretch" }}>
      <a
        id="installLink"
        className={`install-link ${!isValid ? "disabled" : ""}`}
        href={installLink}
        onClick={(e) => !isValid && e.preventDefault()}
        style={{ flex: 1 }}
      >
        Install Addon
      </a>
      <button
        type="button"
        className={`install-link ${!isValid ? "disabled" : ""}`}
        disabled={!isValid}
        onClick={onCopy}
        title={isValid ? `Copy ${httpLink}` : "Copy install URL"}
        style={{ minWidth: 90, cursor: isValid ? "pointer" : "not-allowed" }}
      >
        {copied ? "Copied!" : "Copy URL"}
      </button>
    </div>
  );
};

export default InstallActions;
