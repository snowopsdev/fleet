import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import { previewActivity, previewFleet } from "../../lib/preview";
import { ActivityContent, BusyChart } from "./ActivityDetail";

const view = previewFleet();
const activity = previewActivity();

describe("activity page", () => {
  it("renders every window's summary from the demo fixture", () => {
    for (const window of ["24h", "7d", "30d"] as const) {
      const html = renderToStaticMarkup(<ActivityContent a={activity.analytics[window]} view={view} />);
      expect(html).toContain("GitHub-hosted equivalent");
      expect(html).toContain("studio");
      expect(html).toContain("Queue wait by labels");
      expect(html).toContain(`rates as of ${activity.analytics[window].hostedCost.rates.asOf}`);
    }
  });

  it("draws one bar per bucket, scaled to runner capacity", () => {
    const day = activity.analytics["24h"];
    const html = renderToStaticMarkup(<BusyChart a={day} />);
    expect(html.match(/<rect /g)).toHaveLength(day.buckets.length);
    expect(html).toContain('role="img"');
  });
});
