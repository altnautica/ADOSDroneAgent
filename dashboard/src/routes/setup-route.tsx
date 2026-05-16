import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { WizardShell, type WizardStep } from "@/components/wizard/wizard-shell";
import { useStatus } from "@/hooks/use-status";
import {
  finishSetup,
  installCloudflared,
  postCloudChoice,
  postNavigationAssignCamera,
  postNavigationConfig,
  postProfile,
  skipStep,
} from "@/lib/setup-actions";
import type { GroundRole, Profile } from "@/lib/types";

import { CloudPairStep } from "./setup/cloud-pair-step";
import { ConnectivityStep } from "./setup/connectivity-step";
import { FinishStep } from "./setup/finish-step";
import {
  NavigationStep,
  type NavigationState,
} from "./setup/navigation-step";
import { ProfileStep } from "./setup/profile-step";

const STEPS: ReadonlyArray<WizardStep> = [
  {
    id: "profile",
    label: "Profile",
    description:
      "Pick what this device is. Detected hardware is shown so you can confirm or override the auto-detect.",
  },
  {
    id: "connectivity",
    label: "Connectivity",
    description:
      "Sanity-check that MAVLink, video, and the network uplink are healthy before continuing.",
  },
  {
    id: "navigation",
    label: "Navigation",
    description:
      "Optional. Configure optical flow, VIO, or a rangefinder for GPS-denied flight.",
  },
  {
    id: "cloud-pair",
    label: "Cloud + Pair",
    description:
      "Choose a cloud posture (or stay local) and surface the pairing code for Mission Control.",
  },
  {
    id: "finish",
    label: "Finish",
    description:
      "Optional remote access plus a final confirm. The agent flips out of wizard mode after this.",
  },
];

interface ProfileState {
  profile: Profile;
  ground_role?: GroundRole;
  isValid: boolean;
}

interface CloudState {
  mode: "cloud" | "self_hosted" | "local";
  backend_url?: string;
  mqtt_broker?: string;
  mqtt_port?: number;
  api_key?: string;
  isValid: boolean;
}

interface FinishState {
  enableCloudflared: boolean;
  cloudflaredToken?: string;
}

export function SetupRoute() {
  const status = useStatus();
  const navigate = useNavigate();
  const qc = useQueryClient();

  const [stepId, setStepId] = useState<string>("profile");
  const [errorMsg, setErrorMsg] = useState<string | null>(null);

  const [profileState, setProfileState] = useState<ProfileState>({
    profile: "drone",
    isValid: false,
  });
  const [cloudState, setCloudState] = useState<CloudState>({
    mode: "cloud",
    isValid: true,
  });
  const [finishState, setFinishState] = useState<FinishState>({
    enableCloudflared: false,
  });
  const [navState, setNavState] = useState<NavigationState>({
    enableOpticalFlow: false,
    enableVio: false,
    enableRangefinder: false,
    cameraDevice: "",
    rangefinder: {
      topology: "companion",
      driver: "tfluna_uart",
      devicePath: "",
    },
  });

  // Auto-redirect home if setup is already finalized.
  useEffect(() => {
    if (status.data?.setup_finalized) navigate("/", { replace: true });
  }, [status.data?.setup_finalized, navigate]);

  const profileMut = useMutation({
    mutationFn: () =>
      postProfile({
        profile: profileState.profile,
        ground_role: profileState.ground_role,
        source: "user",
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["setup-status"] }),
  });

  const cloudMut = useMutation({
    mutationFn: () =>
      postCloudChoice({
        mode: cloudState.mode,
        backend_url: cloudState.backend_url,
        mqtt_broker: cloudState.mqtt_broker,
        mqtt_port: cloudState.mqtt_port,
        api_key: cloudState.api_key,
      }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["setup-status"] }),
  });

  const cloudflaredMut = useMutation({
    mutationFn: () =>
      installCloudflared({ token: finishState.cloudflaredToken }),
  });

  const finishMut = useMutation({
    mutationFn: finishSetup,
    onSuccess: () => qc.invalidateQueries({ queryKey: ["setup-status"] }),
  });

  const idx = STEPS.findIndex((s) => s.id === stepId);
  const step = STEPS[idx];

  const onCloudChange = useCallback((next: CloudState) => setCloudState(next), []);
  const onFinishChange = useCallback((next: FinishState) => setFinishState(next), []);
  const onProfileChange = useCallback(
    (next: ProfileState) => setProfileState(next),
    [],
  );
  const onNavChange = useCallback((next: NavigationState) => setNavState(next), []);

  function deriveNavMode(s: NavigationState): "off" | "optical-flow" | "vio" | "both" {
    if (s.enableOpticalFlow && s.enableVio) return "both";
    if (s.enableVio) return "vio";
    if (s.enableOpticalFlow) return "optical-flow";
    return "off";
  }

  async function applyNavigationStep() {
    const mode = deriveNavMode(navState);
    if (mode === "off" && !navState.enableRangefinder) {
      // Pure skip path. Mark the wizard step deferred and move on.
      await skipStep("navigation").catch(() => undefined);
      return;
    }
    if (mode !== "off" && navState.cameraDevice) {
      await postNavigationAssignCamera({
        device_path: navState.cameraDevice,
        role: "nav",
      });
    }
    await postNavigationConfig({
      mode,
      rangefinder: navState.enableRangefinder
        ? {
            topology: navState.rangefinder.topology,
            driver: navState.rangefinder.driver,
            device: {
              path: navState.rangefinder.devicePath,
              baud: navState.rangefinder.baud,
              address: navState.rangefinder.address,
            },
          }
        : undefined,
    });
  }

  const goNext = async () => {
    setErrorMsg(null);
    try {
      if (stepId === "profile") {
        await profileMut.mutateAsync();
        setStepId("connectivity");
      } else if (stepId === "connectivity") {
        setStepId("navigation");
      } else if (stepId === "navigation") {
        await applyNavigationStep();
        await qc.invalidateQueries({ queryKey: ["setup-status"] });
        setStepId("cloud-pair");
      } else if (stepId === "cloud-pair") {
        await cloudMut.mutateAsync();
        setStepId("finish");
      } else if (stepId === "finish") {
        if (finishState.enableCloudflared) {
          await cloudflaredMut.mutateAsync().catch((err) => {
            // Cloudflared install is optional. Surface the error but
            // don't block the wizard from completing.
            const msg = err instanceof Error ? err.message : String(err);
            setErrorMsg(`remote access install failed: ${msg}`);
          });
        }
        await finishMut.mutateAsync();
        // Force the setup-status cache to refresh BEFORE navigating
        // so IndexRedirect sees setup_finalized=true and routes
        // straight to /home (no flash of the wizard).
        await qc.invalidateQueries({
          queryKey: ["setup-status"],
          refetchType: "all",
        });
        navigate("/home", { replace: true });
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      setErrorMsg(msg);
    }
  };

  const goBack = () => {
    setErrorMsg(null);
    if (idx > 0) setStepId(STEPS[idx - 1].id);
  };

  const nextDisabled =
    (stepId === "profile" && !profileState.isValid) ||
    (stepId === "cloud-pair" && !cloudState.isValid);

  const nextLoading =
    profileMut.isPending ||
    cloudMut.isPending ||
    cloudflaredMut.isPending ||
    finishMut.isPending;

  const nextLabel = stepId === "finish" ? "Finish" : "Next";

  if (!step) return null;

  return (
    <WizardShell
      steps={STEPS}
      currentStepId={stepId}
      onChangeStep={setStepId}
      onBack={idx > 0 ? goBack : undefined}
      backDisabled={idx === 0 || nextLoading}
      onNext={goNext}
      nextDisabled={nextDisabled || nextLoading}
      nextLoading={nextLoading}
      nextLabel={nextLabel}
      rightAction={
        <Button asChild variant="ghost" size="sm">
          <Link to="/">Skip to Home</Link>
        </Button>
      }
    >
      {stepId === "profile" && <ProfileStep onChange={onProfileChange} />}
      {stepId === "connectivity" && <ConnectivityStep />}
      {stepId === "navigation" && <NavigationStep onChange={onNavChange} />}
      {stepId === "cloud-pair" && <CloudPairStep onChange={onCloudChange} />}
      {stepId === "finish" && <FinishStep onChange={onFinishChange} />}

      {errorMsg && (
        <Card>
          <CardContent className="pt-4">
            <p className="text-sm text-destructive">{errorMsg}</p>
          </CardContent>
        </Card>
      )}
    </WizardShell>
  );
}
