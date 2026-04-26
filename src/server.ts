import express from "express";
import { router as nzbHydraRouter } from "./nzbhydra/router.js";
import { router as nzbRouter } from "./nzb/router.js";
import {
  indexerHealthcheckHandler,
  nntpHealthcheckHandler,
} from "./healthcheck.js";

const app = express();
const port = process.env.PORT ? +process.env.PORT : 3000;

// Middleware
app.use(express.json());

// Routes
app.use("/nzbhydra", nzbHydraRouter);
app.use("/nzb", nzbRouter);
app.use(express.static("public"));

// Healthcheck endpoints
app.post("/api/healthcheck/indexer", indexerHealthcheckHandler);
app.post("/api/healthcheck/nntp", nntpHealthcheckHandler);

app.listen(port, () =>
  console.log(`Addon listening at http://localhost:${port}`),
);
