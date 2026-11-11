import { version } from "react";
import { sum } from "./sum";

document.getElementById("root")!.textContent = `react ${version}, ${sum(1, 1)}`;
