
# emacs and viins only: in vicmd the arrows are vi's own history motion, and
# taking them over there would override a native binding rather than restore one.
for _zhis_km in emacs viins; do
	bindkey -M $_zhis_km '^[[A' _fhistory_widget
	bindkey -M $_zhis_km '^[OA' _fhistory_widget
	bindkey -M $_zhis_km '^[[B' _fhistory_widget
	bindkey -M $_zhis_km '^[OB' _fhistory_widget
done
unset _zhis_km
