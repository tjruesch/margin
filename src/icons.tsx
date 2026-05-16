import type { ReactNode } from "react";
import { siFigma, siGithub, siLinear, siNotion } from "simple-icons";

type IconProps = {
  size?: number;
  /** Stroke width. */
  sw?: number;
  children: ReactNode;
};

function Icon({ size = 16, sw = 1.6, children }: IconProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={sw}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {children}
    </svg>
  );
}

type Props = Omit<IconProps, "children">;

export const IconHome = (p: Props) => (
  <Icon {...p}>
    <path d="M3 11.5L12 4l9 7.5" />
    <path d="M5 10.5V20h14v-9.5" />
    <path d="M10 20v-5h4v5" />
  </Icon>
);
export const IconChevLeft = (p: Props) => (
  <Icon {...p}>
    <path d="M15 6l-6 6 6 6" />
  </Icon>
);
export const IconEdit = (p: Props) => (
  <Icon {...p}>
    <path d="M4 20h4l11-11-4-4L4 16v4z" />
    <path d="M14 5l4 4" />
  </Icon>
);
export const IconEye = (p: Props) => (
  <Icon {...p}>
    <path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z" />
    <circle cx="12" cy="12" r="2.6" />
  </Icon>
);
export const IconSettings = (p: Props) => (
  <Icon {...p}>
    <circle cx="12" cy="12" r="2.5" />
    <path d="M12 3v2M12 19v2M3 12h2M19 12h2M5.6 5.6l1.4 1.4M17 17l1.4 1.4M5.6 18.4L7 17M17 7l1.4-1.4" />
  </Icon>
);
export const IconShare = (p: Props) => (
  <Icon {...p}>
    <path d="M12 4v12" />
    <path d="M8 8l4-4 4 4" />
    <path d="M5 14v5a1 1 0 001 1h12a1 1 0 001-1v-5" />
  </Icon>
);
export const IconLink = (p: Props) => (
  <Icon {...p}>
    <path d="M10 14a4 4 0 005.6 0l3-3a4 4 0 10-5.6-5.6l-1 1" />
    <path d="M14 10a4 4 0 00-5.6 0l-3 3a4 4 0 105.6 5.6l1-1" />
  </Icon>
);

/// Per-kind brand glyphs for workstream link chips. Maps each
/// canonical `link_kinds::*` value to its Simple Icons path; falls
/// back to the generic `IconLink` for `other` / null / unknown.
/// Brand path uses `currentColor` so the chip's own color rules drive
/// the glyph — keeps the palette consistent with the rest of the UI.
const BRAND_PATHS: Record<string, string> = {
  github: siGithub.path,
  linear: siLinear.path,
  notion: siNotion.path,
  figma: siFigma.path,
};

export const IconBrand = ({
  kind,
  size = 12,
}: {
  kind: string | null;
  size?: number;
}) => {
  const path = kind ? BRAND_PATHS[kind] : undefined;
  if (!path) return <IconLink size={size} sw={1.8} />;
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="currentColor"
      aria-hidden="true"
    >
      <path d={path} />
    </svg>
  );
};
export const IconBriefcase = (p: Props) => (
  <Icon {...p}>
    <rect x="3" y="7" width="18" height="13" rx="2" />
    <path d="M9 7V5a2 2 0 012-2h2a2 2 0 012 2v2" />
    <path d="M3 12h18" />
  </Icon>
);
export const IconMore = (p: Props) => (
  <Icon {...p}>
    <circle cx="5" cy="12" r="1.2" />
    <circle cx="12" cy="12" r="1.2" />
    <circle cx="19" cy="12" r="1.2" />
  </Icon>
);
export const IconStar = (p: Props) => (
  <Icon {...p}>
    <path
      d="M12 3.5l2.7 5.6 6.1.9-4.4 4.3 1 6.1L12 17.6 6.6 20.4l1-6.1L3.2 10l6.1-.9L12 3.5z"
      fill="currentColor"
    />
  </Icon>
);
export const IconCopy = (p: Props) => (
  <Icon {...p}>
    <rect x="8" y="8" width="12" height="12" rx="2" />
    <path d="M16 8V6a2 2 0 00-2-2H6a2 2 0 00-2 2v8a2 2 0 002 2h2" />
  </Icon>
);
export const IconArchive = (p: Props) => (
  <Icon {...p}>
    <rect x="3.5" y="4.5" width="17" height="4" rx="1" />
    <path d="M5 8.5V19a1 1 0 001 1h12a1 1 0 001-1V8.5" />
    <path d="M10 13h4" />
  </Icon>
);
export const IconTrash = (p: Props) => (
  <Icon {...p}>
    <path d="M4 7h16" />
    <path d="M9 7V5a1 1 0 011-1h4a1 1 0 011 1v2" />
    <path d="M6 7l1 13a1 1 0 001 1h8a1 1 0 001-1l1-13" />
  </Icon>
);
export const IconSparkle = (p: Props) => (
  <Icon {...p}>
    <path d="M12 4l1.6 4.4L18 10l-4.4 1.6L12 16l-1.6-4.4L6 10l4.4-1.6L12 4z" />
    <path d="M19 16l.7 1.8L21.5 18.5l-1.8.7L19 21l-.7-1.8L16.5 18.5l1.8-.7L19 16z" />
  </Icon>
);
export const IconFolder = (p: Props) => (
  <Icon {...p}>
    <path d="M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2V7z" />
  </Icon>
);
export const IconCalendar = (p: Props) => (
  <Icon {...p}>
    <rect x="3.5" y="5" width="17" height="15" rx="2" />
    <path d="M8 3v4M16 3v4M3.5 10h17" />
  </Icon>
);
export const IconMail = (p: Props) => (
  <Icon {...p}>
    <rect x="3" y="5" width="18" height="14" rx="2" />
    <path d="M3.5 6.5l8.5 6.5 8.5-6.5" />
  </Icon>
);
export const IconChat = (p: Props) => (
  <Icon {...p}>
    <path d="M4 5h16a1 1 0 011 1v10a1 1 0 01-1 1h-9l-4 3v-3H4a1 1 0 01-1-1V6a1 1 0 011-1z" />
  </Icon>
);
export const IconPlus = (p: Props) => (
  <Icon {...p}>
    <path d="M12 5v14M5 12h14" />
  </Icon>
);
export const IconFileText = (p: Props) => (
  <Icon {...p}>
    <path d="M14 3H7a2 2 0 00-2 2v14a2 2 0 002 2h10a2 2 0 002-2V8l-5-5z" />
    <path d="M14 3v5h5" />
    <path d="M9 13h6M9 17h6M9 9h2" />
  </Icon>
);
export const IconChecklist = (p: Props) => (
  <Icon {...p}>
    <path d="M4 6l1.5 1.5L8 5" />
    <path d="M4 12l1.5 1.5L8 10" />
    <path d="M4 18l1.5 1.5L8 16" />
    <path d="M11 6h9M11 12h9M11 18h9" />
  </Icon>
);
export const IconUsers = (p: Props) => (
  <Icon {...p}>
    <circle cx="9" cy="8" r="3.2" />
    <path d="M3 20c0-3 2.7-5 6-5s6 2 6 5" />
    <path d="M16 11a3 3 0 000-6" />
    <path d="M21 20c0-2.5-2-4.5-4.5-5" />
  </Icon>
);
export const IconUser = (p: Props) => (
  <Icon {...p}>
    <circle cx="12" cy="8" r="3.6" />
    <path d="M5 20c0-3.5 3.1-6 7-6s7 2.5 7 6" />
  </Icon>
);
export const IconSearch = (p: Props) => (
  <Icon {...p}>
    <circle cx="11" cy="11" r="6" />
    <path d="M20 20l-4.5-4.5" />
  </Icon>
);
export const IconBell = (p: Props) => (
  <Icon {...p}>
    <path d="M6 8a6 6 0 1112 0v5l1.5 2.5h-15L6 13V8z" />
    <path d="M10 19a2 2 0 004 0" />
  </Icon>
);
export const IconSidebar = (p: Props) => (
  <Icon {...p}>
    <rect x="3" y="5" width="18" height="14" rx="2" />
    <path d="M9 5v14" />
  </Icon>
);
export const IconChevRight = (p: Props) => (
  <Icon {...p}>
    <path d="M9 6l6 6-6 6" />
  </Icon>
);
export const IconArrowRight = (p: Props) => (
  <Icon {...p}>
    <path d="M5 12h14M13 6l6 6-6 6" />
  </Icon>
);
export const IconMic = (p: Props) => (
  <Icon {...p}>
    <rect x="9" y="3" width="6" height="11" rx="3" />
    <path d="M5 11a7 7 0 0014 0" />
    <path d="M12 18v3M9 21h6" />
  </Icon>
);
export const IconCheck = (p: Props) => (
  <Icon {...p}>
    <path d="M2 13l7 7L22 4" />
  </Icon>
);

/** Question-mark glyph for the Open Questions surface (#113). */
export const IconHelp = (p: Props) => (
  <Icon {...p}>
    <circle cx="12" cy="12" r="9" />
    <path d="M9.5 9a2.5 2.5 0 0 1 5 0c0 1.5-2.5 2-2.5 4" />
    <circle cx="12" cy="17" r="0.6" fill="currentColor" />
  </Icon>
);
