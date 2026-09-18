import type { Preview, Decorator } from "@storybook/react-vite";
import { useEffect } from "react";
import "../src/styles/index.css";

/**
 * Theme toolbar (light/dark), applied via `data-theme` the same way the app
 * shell applies an operator's choice (src/lib/theme.ts). Storybook renders
 * every story in both themes are checked manually via this toolbar; axe runs
 * against whichever theme is active (accessibility.md).
 */
const ThemeDecorator: Decorator = (Story, context) => {
  const theme = context.globals.theme as "light" | "dark";
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
  }, [theme]);
  return (
    <div className="bg-canvas p-6 text-text" data-theme={theme}>
      <Story />
    </div>
  );
};

const preview: Preview = {
  parameters: {
    controls: {
      matchers: {
        color: /(background|color)$/i,
        date: /Date$/i,
      },
    },
    // accessibility.md: axe runs on every story at these WCAG tag levels.
    a11y: {
      config: {},
      options: {
        runOnly: {
          type: "tag",
          values: ["wcag2a", "wcag2aa", "wcag21aa", "wcag22aa"],
        },
      },
      test: "error",
    },
    layout: "centered",
  },
  globalTypes: {
    theme: {
      description: "Theme",
      toolbar: {
        title: "Theme",
        icon: "mirror",
        items: [
          { value: "light", title: "Light" },
          { value: "dark", title: "Dark" },
        ],
        dynamicTitle: true,
      },
    },
  },
  initialGlobals: {
    theme: "light",
  },
  decorators: [ThemeDecorator],
};

export default preview;
