import express from "express";
import { router as nzbHydraRouter } from "./nzbhydra/router.js";
import { router as nzbRouter } from "./nzb/router.js";

const app = express();
const port = process.env.PORT ? +process.env.PORT : 3000;

app.use("/nzbhydra", nzbHydraRouter);
app.use("/nzb", nzbRouter);

app.listen(port, () => {
  console.log(`Addon listening at http://localhost:${port}`);
});
