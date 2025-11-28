import express from "express";
import { router } from "./nzbhydra/router.js";

const app = express();
const port = process.env.PORT ? +process.env.PORT : 3000;

app.use("/nzbhydra", router);

app.listen(port, () => {
  console.log(`Addon listening at http://localhost:${port}`);
});
