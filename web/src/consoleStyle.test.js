import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, it } from "vitest";

const source = readFileSync(join(process.cwd(), "public/console-v2/index.html"), "utf8");
const sha256 = (value) => createHash("sha256").update(value).digest("hex");

it("retains the Console v3 theme CSS and every original inline style", () => {
  const theme = [...source.matchAll(/<style>([\s\S]*?)<\/style>/g)]
    .map((match) => match[1])
    .find((block) => block.includes("[data-theme]"));
  const inlineStyles = [...source.matchAll(/\bstyle="([^"]*)"/g)]
    .map((match) => match[1]);

  expect(sha256(theme)).toBe("e9608a21b4006865b19b0477cdba174bca4377d5e26ee0e0fb90464e463d1462");
  expect(inlineStyles).toHaveLength(827);
  expect(sha256(inlineStyles.join("\n"))).toBe("ba8d428361ab8bd99a313a44eef87bfffdd751ca72fc06874ace69cf43cb2a0e");
});
