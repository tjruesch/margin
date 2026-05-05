import type { ReactNode } from "react";

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
export const IconMore = (p: Props) => (
  <Icon {...p}>
    <circle cx="5" cy="12" r="1.2" />
    <circle cx="12" cy="12" r="1.2" />
    <circle cx="19" cy="12" r="1.2" />
  </Icon>
);
export const IconStar = (p: Props) => (
  <Icon {...p}>
    <path d="M12 3.5l2.7 5.6 6.1.9-4.4 4.3 1 6.1L12 17.6 6.6 20.4l1-6.1L3.2 10l6.1-.9L12 3.5z" />
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
export const IconPlus = (p: Props) => (
  <Icon {...p}>
    <path d="M12 5v14M5 12h14" />
  </Icon>
);
