import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, it } from "vitest";

const source = readFileSync(join(process.cwd(), "public/console-v2/index.html"), "utf8");
const sha256 = (value) => createHash("sha256").update(value).digest("hex");

it("retains the Console v2 theme CSS and every original inline style", () => {
  const theme = [...source.matchAll(/<style>([\s\S]*?)<\/style>/g)]
    .map((match) => match[1])
    .find((block) => block.includes("[data-theme]"));
  const inlineStyles = [...source.matchAll(/\bstyle="([^"]*)"/g)]
    .map((match) => match[1]);

  expect(sha256(theme)).toBe("c144d89352295cd3ee413e58284b49634f225b22eaa7e8af3972f65ad8601b52");
  expect(inlineStyles).toHaveLength(629);
  expect(sha256(inlineStyles.join("\n"))).toBe("796906004fff8f6ab214eeee38a4f658c0732d9cf64c04fea4d237366236989c");
});
