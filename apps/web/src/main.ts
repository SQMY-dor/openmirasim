export interface WorkbenchStatus {
  readonly name: string;
  readonly phase: string;
}

export const WORKBENCH_STATUS: WorkbenchStatus = {
  name: "OpenMirasim Workbench",
  phase: "phase-0-scaffold",
};

const root = document.getElementById("root");
if (root) {
  root.textContent = `${WORKBENCH_STATUS.name} (${WORKBENCH_STATUS.phase})`;
}
