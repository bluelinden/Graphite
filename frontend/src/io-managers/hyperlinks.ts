import { type Editor } from "@graphite/editor";
import { TriggerVisitLink, TriggerOpenEmbeddedFile } from "@graphite/messages";

export function createHyperlinkManager(editor: Editor) {
	// Subscribe to process backend event
	editor.subscriptions.subscribeJsMessage(TriggerVisitLink, async (triggerOpenLink) => {
		window.open(triggerOpenLink.url, "_blank");
	});
	editor.subscriptions.subscribeJsMessage(TriggerOpenEmbeddedFile, async (triggerOpenEmbeddedFile) => {
		window.open("/" + triggerOpenEmbeddedFile.path, "_blank");
	});
}
