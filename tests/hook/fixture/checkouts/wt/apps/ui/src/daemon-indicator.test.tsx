import { render } from "./render";
import { DaemonIndicator } from "./daemon-indicator";

describe("DaemonIndicator", () => {
  it("shows the running state", () => {
    const view = render(<DaemonIndicator state="running" />);
    expect(view.text()).toContain("running");
  });

  it("shows the stopped state", () => {
    const view = render(<DaemonIndicator state="stopped" />);
    expect(view.text()).toContain("stopped");
  });
});
