import { initializeBindings } from "./bindings.js";
import { handleAuthFailure, handleEnvelope } from "./handlers.js";
import { initializeShell } from "./shell.js";
import { initializeTheme } from "./theme.js";
import { boot } from "./transport.js";

initializeTheme();
initializeBindings({ handleEnvelope, handleAuthFailure });
initializeShell();
boot({ handleEnvelope, handleAuthFailure });
