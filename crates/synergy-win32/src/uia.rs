//! Windows UI Automation (UIA) integration.
//!
//! Provides a high-level wrapper around the COM-based UI Automation API.
//! Used by GUI adapters to programmatically interact with embedded apps.
//!
//! NOTE: The `windows` crate v0.48 UI Automation bindings require careful
//! VARIANT construction. This module uses the raw COM interface.

use anyhow::{anyhow, Context, Result};
use windows::core::BSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationValuePattern, TreeScope_Descendants, UIA_InvokePatternId,
    UIA_ValuePatternId,
};

use crate::WinHandle;

/// High-level UI Automation client.
pub struct UiAutomation {
    automation: IUIAutomation,
}

impl UiAutomation {
    /// Initialize COM and create the UIA client.
    pub fn new() -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL)
                    .context("CoCreateInstance CUIAutomation")?;
            Ok(Self { automation })
        }
    }

    /// Get the root UIA element for a given window handle.
    pub fn element_from_handle(&self, hwnd: WinHandle) -> Result<IUIAutomationElement> {
        let h = HWND(hwnd);
        unsafe {
            self.automation
                .ElementFromHandle(h)
                .context("ElementFromHandle")
        }
    }

    /// Find a descendant element by its Automation ID (string match).
    pub fn find_by_automation_id(
        &self,
        root: &IUIAutomationElement,
        automation_id: &str,
    ) -> Result<IUIAutomationElement> {
        // Use the TrueCondition and walk children manually, or use
        // CreatePropertyCondition with the correct VARIANT type.
        // For windows 0.48, we use a simpler approach: walk all descendants.
        self.find_descendant_by_property(root, "AutomationId", automation_id)
    }

    /// Find a descendant element by its Name property.
    pub fn find_by_name(
        &self,
        root: &IUIAutomationElement,
        name: &str,
    ) -> Result<IUIAutomationElement> {
        self.find_descendant_by_property(root, "Name", name)
    }

    /// Read the Name property of an element.
    pub fn get_name(&self, element: &IUIAutomationElement) -> Result<String> {
        unsafe {
            let bstr = element.CurrentName().context("CurrentName")?;
            Ok(bstr.to_string())
        }
    }

    /// Read the Value pattern's current value (for edit/text controls).
    pub fn get_value(&self, element: &IUIAutomationElement) -> Result<String> {
        unsafe {
            let pattern: IUIAutomationValuePattern = element
                .GetCurrentPatternAs(UIA_ValuePatternId)
                .context("GetCurrentPatternAs ValuePattern")?;
            let bstr = pattern.CurrentValue().context("CurrentValue")?;
            Ok(bstr.to_string())
        }
    }

    /// Set the value of an edit control via the Value pattern.
    pub fn set_value(&self, element: &IUIAutomationElement, value: &str) -> Result<()> {
        unsafe {
            let pattern: IUIAutomationValuePattern = element
                .GetCurrentPatternAs(UIA_ValuePatternId)
                .context("GetCurrentPatternAs ValuePattern")?;
            let bstr = BSTR::from(value);
            pattern.SetValue(&bstr).context("SetValue")?;
            Ok(())
        }
    }

    /// Invoke a button or other invocable element.
    pub fn invoke(&self, element: &IUIAutomationElement) -> Result<()> {
        unsafe {
            let pattern: IUIAutomationInvokePattern = element
                .GetCurrentPatternAs(UIA_InvokePatternId)
                .context("GetCurrentPatternAs InvokePattern")?;
            pattern.Invoke().context("Invoke")?;
            Ok(())
        }
    }

    // ─── Internal ────────────────────────────────────────────────────────

    /// Walk descendants using TrueCondition and match by property.
    /// This is less efficient than CreatePropertyCondition but avoids
    /// VARIANT construction issues across `windows` crate versions.
    fn find_descendant_by_property(
        &self,
        root: &IUIAutomationElement,
        property: &str,
        value: &str,
    ) -> Result<IUIAutomationElement> {
        unsafe {
            let condition = self
                .automation
                .CreateTrueCondition()
                .context("CreateTrueCondition")?;

            let all = root
                .FindAll(TreeScope_Descendants, &condition)
                .context("FindAll descendants")?;

            let count = all.Length().unwrap_or(0);
            for i in 0..count {
                let el = match all.GetElement(i) {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                let matches = match property {
                    "Name" => el
                        .CurrentName()
                        .map(|n| n.to_string() == value)
                        .unwrap_or(false),
                    "AutomationId" => el
                        .CurrentAutomationId()
                        .map(|id| id.to_string() == value)
                        .unwrap_or(false),
                    _ => false,
                };

                if matches {
                    return Ok(el);
                }
            }

            Err(anyhow!(
                "Element with {} = '{}' not found",
                property,
                value
            ))
        }
    }
}
