/**
 * MDX globals registry — components available inside MDX without `import`.
 * Wired via `<Content components={components} />` in `[...slug].astro`.
 * Add new components here as you build (or install) them.
 */

import { Aside } from "./components/ui/aside";
import Render from "./components/Render.astro";
import { Card } from "./components/ui/card";
import { CardGrid } from "./components/ui/card-grid";
import { PackageManagers } from "./components/ui/package-managers";
import { Step, Steps } from "./components/ui/steps";
import { Tabs, TabItem } from "./components/ui/tabs";
import OcHubAppFigure from "./components/OcHubAppFigure.astro";
import OcHubConnectionMap from "./components/OcHubConnectionMap.astro";
import OcHubStepsVisual from "./components/OcHubStepsVisual.astro";

export const components = {
  Aside,
  Card,
  CardGrid,
  PackageManagers,
  OcHubAppFigure,
  OcHubConnectionMap,
  OcHubStepsVisual,
  Render,
  Step,
  Steps,
  TabItem,
  Tabs,
};
