import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { VIEWS, GROUP_LABEL } from "./registry";
import { Badge } from "./components";
import "./styles.css";

// OSS entrypoint: the OSS registry only. The edition badge is authored here (in the
// OSS build) so no cross-edition labelling lives in the shared shell.
createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App
      views={VIEWS}
      groupLabels={GROUP_LABEL}
      editionBadge={() => <Badge tone="default">OSS · local</Badge>}
    />
  </StrictMode>
);
