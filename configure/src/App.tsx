import { useMemo } from "react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import ArrayField from "./components/ArrayField";
import InstallActions from "./components/InstallActions";
import PasswordField from "./components/PasswordField";
import type { AddonData } from "./types";
import {
  nzbConfigSchema,
  nzbHydraConfigSchema,
  type NzbConfig,
  type NzbHydraConfig,
} from "./schemas";

/** Default addon data for development/fallback */
const DEFAULT_ADDON_DATA: AddonData = {
  manifest: {
    id: "stremio-nzb-addon",
    name: "NZB Addon",
    version: "0.0.0",
    description: "Stream from Usenet via NZB indexers",
    logo: "https://dl.strem.io/addon-logo.png",
    types: ["movie", "series"],
  },
  config: { fields: [] },
  basePath: "/nzb",
};

/** Get addon data from window or use defaults */
const getAddonData = (): AddonData =>
  window.__ADDON_DATA__ || DEFAULT_ADDON_DATA;

/** Stylize content types for display */
const stylizeType = (type: string): string => {
  const capitalized = type[0].toUpperCase() + type.slice(1);
  return type === "series" ? capitalized : capitalized + "s";
};

/** Determine if this is NZBHydra config (single indexer) or NZB config (multiple) */
const isNzbHydraConfig = (basePath: string): boolean =>
  basePath.includes("nzbhydra");

/**
 * NZBHydra configuration form (single indexer)
 */
const NzbHydraForm = ({ basePath }: { basePath: string }) => {
  const {
    register,
    control,
    watch,
    getValues,
    formState: { errors, isValid },
  } = useForm<NzbHydraConfig>({
    resolver: zodResolver(nzbHydraConfigSchema) as any,
    mode: "onChange",
    defaultValues: {
      indexerUrl: "",
      indexerApiKey: "",
      nntpServers: [{ server: "" }],
      minGbitPerHour: undefined,
      maxGbitPerHour: undefined,
      excludeRegex: "",
      validateNzbStructure: false,
      validateNzbAvailability: false,
      streamsPerResolution: undefined,
    },
  });

  const formValues = watch();

  const installLink = useMemo(() => {
    const configObj: Record<string, unknown> = {};

    if (formValues.indexerUrl) configObj.indexerUrl = formValues.indexerUrl;
    if (formValues.indexerApiKey)
      configObj.indexerApiKey = formValues.indexerApiKey;
    if (formValues.nntpServers?.length) {
      const servers = formValues.nntpServers.filter((s) => s.server);
      if (servers.length) configObj.nntpServers = servers;
    }
    if (formValues.minGbitPerHour)
      configObj.minGbitPerHour = formValues.minGbitPerHour;
    if (formValues.maxGbitPerHour)
      configObj.maxGbitPerHour = formValues.maxGbitPerHour;
    if (formValues.excludeRegex?.trim())
      configObj.excludeRegex = formValues.excludeRegex.trim();
    if (formValues.validateNzbStructure)
      configObj.validateNzbStructure = true;
    if (formValues.validateNzbAvailability)
      configObj.validateNzbAvailability = true;
    if (formValues.streamsPerResolution)
      configObj.streamsPerResolution = formValues.streamsPerResolution;

    const hasConfig = Object.keys(configObj).length > 0;
    const configPart = hasConfig
      ? `/${encodeURIComponent(JSON.stringify(configObj))}`
      : "";

    return `stremio://${window.location.host}${basePath}${configPart}/manifest.json`;
  }, [formValues, basePath]);

  return (
    <>
      <form id="mainForm" onSubmit={(e) => e.preventDefault()}>
        <h3>Configuration</h3>

        <div className="form-element">
          <div className="label-to-top">
            Indexer URL <span style={{ color: "red" }}>*</span>
          </div>
          <input
            type="text"
            {...register("indexerUrl")}
            placeholder="https://your-nzbhydra-instance.com"
            className="full-width"
          />
          {errors.indexerUrl && (
            <span className="validation-error">
              {errors.indexerUrl.message}
            </span>
          )}
        </div>

        <div className="form-element">
          <div className="label-to-top">
            API Key <span style={{ color: "red" }}>*</span>
          </div>
          <PasswordField
            {...register("indexerApiKey")}
            placeholder="Your NZBHydra API key"
          />
          {errors.indexerApiKey && (
            <span className="validation-error">
              {errors.indexerApiKey.message}
            </span>
          )}
        </div>

        <ArrayField
          name="nntpServers"
          control={control}
          errors={errors}
          register={register}
          getValues={getValues}
          title="NNTP Server"
          type="nntpServers"
          required
        />

        <div className="form-element">
          <div className="label-to-top">Bandwidth Filter (Gbit/hour)</div>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              type="number"
              {...register("minGbitPerHour")}
              placeholder="Min (e.g. 5)"
              className="full-width"
              step="0.1"
              min="0"
            />
            <input
              type="number"
              {...register("maxGbitPerHour")}
              placeholder="Max (e.g. 25)"
              className="full-width"
              step="0.1"
              min="0"
            />
          </div>
          {errors.minGbitPerHour && (
            <span className="validation-error">
              Min: {errors.minGbitPerHour.message}
            </span>
          )}
          {errors.maxGbitPerHour && (
            <span className="validation-error">
              Max: {errors.maxGbitPerHour.message}
            </span>
          )}
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            Filters by bitrate. Min cuts out fakes/low-quality rips, Max
            caps file size. Typical 1080p ~25 Gbit/hr, 4K ~80 Gbit/hr. Both
            optional.
          </p>
        </div>

        <div className="form-element">
          <div className="label-to-top">Exclude Regex (Title Filter)</div>
          <input
            type="text"
            {...register("excludeRegex")}
            placeholder="e.g. \\b(av1|hdr|dolby[\\s.\\-_]?vision|cam)\\b"
            className="full-width"
          />
          {errors.excludeRegex && (
            <span className="validation-error">
              {errors.excludeRegex.message}
            </span>
          )}
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            Drops any release whose title matches this pattern.
            Case-insensitive by default; wrap in <code>/…/flags</code> to
            override (e.g. <code>/HDR/g</code>).
          </p>
        </div>

        <div className="form-element">
          <label
            style={{ display: "flex", alignItems: "center", gap: 8 }}
          >
            <input type="checkbox" {...register("validateNzbStructure")} />
            <span>Validate NZB structure (slower, more reliable)</span>
          </label>
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            Fetches each NZB and discards releases with no RAR archive
            (Stremio's streaming engine can only stream RAR-packed content).
            Adds ~50–500ms per search; results cached 24h.
          </p>
        </div>

        <div className="form-element">
          <label
            style={{ display: "flex", alignItems: "center", gap: 8 }}
          >
            <input type="checkbox" {...register("validateNzbAvailability")} />
            <span>Validate NZB article availability (slowest, most reliable)</span>
          </label>
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            STATs the first segment of every <code>.partNN.rar</code>{" "}
            against your NNTP servers. Drops releases whose articles aren't
            on any of your backbones (the &quot;not on any backbones&quot;
            error in Stremio's logs). Adds ~0.3–2s per search; cached 24h.
            Most useful for block-account users with limited retention.
          </p>
        </div>

        <div className="form-element">
          <div className="label-to-top">Streams per Resolution</div>
          <input
            type="number"
            {...register("streamsPerResolution")}
            placeholder="1 (one per 720p / 1080p / 2160p)"
            className="full-width"
            step="1"
            min="1"
          />
          {errors.streamsPerResolution && (
            <span className="validation-error">
              {errors.streamsPerResolution.message}
            </span>
          )}
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            How many alternates to keep per resolution bucket. Default 1
            (cleanest list). Bump to 3 if you want fallback options when a
            stream fails to play.
          </p>
        </div>
      </form>

      <div className="separator" />

      <InstallActions installLink={installLink} isValid={isValid} />
    </>
  );
};

/**
 * NZB configuration form (multiple indexers)
 */
const NzbForm = ({ basePath }: { basePath: string }) => {
  const {
    register,
    control,
    watch,
    getValues,
    formState: { errors, isValid },
  } = useForm<NzbConfig>({
    resolver: zodResolver(nzbConfigSchema) as any,
    mode: "onChange",
    defaultValues: {
      indexers: [{ url: "", apiKey: "" }],
      nntpServers: [{ server: "" }],
      minGbitPerHour: undefined,
      maxGbitPerHour: undefined,
      excludeRegex: "",
      validateNzbStructure: false,
      validateNzbAvailability: false,
      streamsPerResolution: undefined,
    },
  });

  const formValues = watch();

  const installLink = useMemo(() => {
    const configObj: Record<string, unknown> = {};

    if (formValues.indexers?.length) {
      const indexers = formValues.indexers.filter((i) => i.url && i.apiKey);
      if (indexers.length) configObj.indexers = indexers;
    }
    if (formValues.nntpServers?.length) {
      const servers = formValues.nntpServers.filter((s) => s.server);
      if (servers.length) configObj.nntpServers = servers;
    }
    if (formValues.minGbitPerHour)
      configObj.minGbitPerHour = formValues.minGbitPerHour;
    if (formValues.maxGbitPerHour)
      configObj.maxGbitPerHour = formValues.maxGbitPerHour;
    if (formValues.excludeRegex?.trim())
      configObj.excludeRegex = formValues.excludeRegex.trim();
    if (formValues.validateNzbStructure)
      configObj.validateNzbStructure = true;
    if (formValues.validateNzbAvailability)
      configObj.validateNzbAvailability = true;
    if (formValues.streamsPerResolution)
      configObj.streamsPerResolution = formValues.streamsPerResolution;

    const hasConfig = Object.keys(configObj).length > 0;
    const configPart = hasConfig
      ? `/${encodeURIComponent(JSON.stringify(configObj))}`
      : "";

    return `stremio://${window.location.host}${basePath}${configPart}/manifest.json`;
  }, [formValues, basePath]);

  return (
    <>
      <form id="mainForm" onSubmit={(e) => e.preventDefault()}>
        <h3>Configuration</h3>

        <ArrayField
          name="indexers"
          control={control}
          errors={errors}
          register={register}
          getValues={getValues}
          title="Indexer"
          type="indexers"
          required
        />

        <ArrayField
          name="nntpServers"
          control={control}
          errors={errors}
          register={register}
          getValues={getValues}
          title="NNTP Server"
          type="nntpServers"
          required
        />

        <div className="form-element">
          <div className="label-to-top">Bandwidth Filter (Gbit/hour)</div>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              type="number"
              {...register("minGbitPerHour")}
              placeholder="Min (e.g. 5)"
              className="full-width"
              step="0.1"
              min="0"
            />
            <input
              type="number"
              {...register("maxGbitPerHour")}
              placeholder="Max (e.g. 25)"
              className="full-width"
              step="0.1"
              min="0"
            />
          </div>
          {errors.minGbitPerHour && (
            <span className="validation-error">
              Min: {errors.minGbitPerHour.message}
            </span>
          )}
          {errors.maxGbitPerHour && (
            <span className="validation-error">
              Max: {errors.maxGbitPerHour.message}
            </span>
          )}
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            Filters by bitrate. Min cuts out fakes/low-quality rips, Max
            caps file size. Typical 1080p ~25 Gbit/hr, 4K ~80 Gbit/hr. Both
            optional.
          </p>
        </div>

        <div className="form-element">
          <div className="label-to-top">Exclude Regex (Title Filter)</div>
          <input
            type="text"
            {...register("excludeRegex")}
            placeholder="e.g. \\b(av1|hdr|dolby[\\s.\\-_]?vision|cam)\\b"
            className="full-width"
          />
          {errors.excludeRegex && (
            <span className="validation-error">
              {errors.excludeRegex.message}
            </span>
          )}
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            Drops any release whose title matches this pattern.
            Case-insensitive by default; wrap in <code>/…/flags</code> to
            override (e.g. <code>/HDR/g</code>).
          </p>
        </div>

        <div className="form-element">
          <label
            style={{ display: "flex", alignItems: "center", gap: 8 }}
          >
            <input type="checkbox" {...register("validateNzbStructure")} />
            <span>Validate NZB structure (slower, more reliable)</span>
          </label>
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            Fetches each NZB and discards releases with no RAR archive
            (Stremio's streaming engine can only stream RAR-packed content).
            Adds ~50–500ms per search; results cached 24h.
          </p>
        </div>

        <div className="form-element">
          <label
            style={{ display: "flex", alignItems: "center", gap: 8 }}
          >
            <input type="checkbox" {...register("validateNzbAvailability")} />
            <span>Validate NZB article availability (slowest, most reliable)</span>
          </label>
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            STATs the first segment of every <code>.partNN.rar</code>{" "}
            against your NNTP servers. Drops releases whose articles aren't
            on any of your backbones (the &quot;not on any backbones&quot;
            error in Stremio's logs). Adds ~0.3–2s per search; cached 24h.
            Most useful for block-account users with limited retention.
          </p>
        </div>

        <div className="form-element">
          <div className="label-to-top">Streams per Resolution</div>
          <input
            type="number"
            {...register("streamsPerResolution")}
            placeholder="1 (one per 720p / 1080p / 2160p)"
            className="full-width"
            step="1"
            min="1"
          />
          {errors.streamsPerResolution && (
            <span className="validation-error">
              {errors.streamsPerResolution.message}
            </span>
          )}
          <p style={{ fontSize: 12, color: "#666", marginTop: 4 }}>
            How many alternates to keep per resolution bucket. Default 1
            (cleanest list). Bump to 3 if you want fallback options when a
            stream fails to play.
          </p>
        </div>
      </form>

      <div className="separator" />

      <InstallActions installLink={installLink} isValid={isValid} />
    </>
  );
};

/**
 * Main configuration page component
 */
const App = () => {
  const addonData = useMemo(() => getAddonData(), []);
  const { manifest, basePath } = addonData;

  const logo = manifest.logo || "https://dl.strem.io/addon-logo.png";
  const stylizedTypes = manifest.types.map(stylizeType);
  const isHydra = isNzbHydraConfig(basePath);

  return (
    <div id="addon">
      <div className="info-note">
        <strong>Warning: Experimental:</strong> usenet streaming is
        experimental.
        <a
          href="https://blog.stremio.com/stremio-new-stream-sources-usenet-rar-zip-ftp-and-more/"
          target="_blank"
          rel="noopener noreferrer"
        >
          Learn more about Usenet support in Stremio
        </a>
      </div>

      <div className="header">
        <div className="logo">
          <img src={logo} alt={manifest.name} />
        </div>
        <h1>
          <span className="accent">{manifest.name}</span>
        </h1>
        <h2 className="description">{manifest.description}</h2>
      </div>

      <h3>Features</h3>
      <div className="features-list">
        <ul>
          {stylizedTypes.map((type) => (
            <li key={type}>{type}</li>
          ))}
        </ul>
      </div>

      <div className="separator" />

      {isHydra ? (
        <NzbHydraForm basePath={basePath} />
      ) : (
        <NzbForm basePath={basePath} />
      )}

      <div className="version">v{manifest.version}</div>

      {manifest.contactEmail && (
        <div className="contact">
          <p>Contact {manifest.name} creator:</p>
          <a href={`mailto:${manifest.contactEmail}`}>
            {manifest.contactEmail}
          </a>
        </div>
      )}

      <a
        className="github"
        href="https://github.com/sleeyax/stremio-nzb-addon"
        target="_blank"
        rel="noopener noreferrer"
      >
        GitHub
      </a>
    </div>
  );
};

export default App;
