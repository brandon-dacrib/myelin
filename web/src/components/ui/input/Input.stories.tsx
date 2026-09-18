import type { Meta, StoryObj } from "@storybook/react-vite";
import { Field, Input, Textarea } from "./Input";

const meta: Meta = { title: "Primitives/Input" };
export default meta;

export const Basic: StoryObj = {
  render: () => (
    <div className="w-80">
      <Field label="Bridge ID" hint="Lowercase letters, digits and hyphens only.">
        {(fieldProps) => <Input {...fieldProps} placeholder="whatsapp" />}
      </Field>
    </div>
  ),
};

export const WithError: StoryObj = {
  render: () => (
    <div className="w-80">
      <Field
        label="Bridge ID"
        error='An appservice with id "whatsapp" is already registered.'
        required
      >
        {(fieldProps) => <Input {...fieldProps} defaultValue="whatsapp" />}
      </Field>
    </div>
  ),
};

export const Disabled: StoryObj = {
  render: () => (
    <div className="w-80">
      <Field label="Sender localpart">
        {(fieldProps) => <Input {...fieldProps} defaultValue="whatsappbot" disabled />}
      </Field>
    </div>
  ),
};

export const TextareaStory: StoryObj = {
  name: "Textarea",
  render: () => (
    <div className="w-80">
      <Field label="Reason" hint="Recorded in the audit log.">
        {(fieldProps) => <Textarea {...fieldProps} placeholder="Repeated spam reports" />}
      </Field>
    </div>
  ),
};
